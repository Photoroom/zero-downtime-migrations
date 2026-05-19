//! Zero-Downtime Migrations CLI
//!
//! A PostgreSQL migration safety linter for Django.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colored::Colorize;

use zero_downtime_migrations::ast::Migration;
use zero_downtime_migrations::config::Config;
use zero_downtime_migrations::diagnostics::{Diagnostic, Severity};
use zero_downtime_migrations::discovery;
use zero_downtime_migrations::error::{Error, Result};
use zero_downtime_migrations::git::{ChangedKind, DiffSource, GitRepo};
use zero_downtime_migrations::rules::{ChangesetRuleRegistry, RuleRegistry};

/// Zero-Downtime Migrations - A PostgreSQL migration safety linter for Django
#[derive(Parser, Debug)]
#[command(name = "zdm")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Paths to lint (files or directories)
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Compare against a git reference (branch, tag, or commit)
    #[arg(long, value_name = "REF")]
    diff: Option<String>,

    /// Compare staged changes against a git reference (for pre-commit hooks)
    #[arg(long, value_name = "REF", conflicts_with = "diff")]
    diff_staged: Option<String>,

    /// Output format
    #[arg(long, value_enum, default_value = "default")]
    output_format: OutputFormat,

    /// Select specific rules to run (comma-separated)
    #[arg(long, value_delimiter = ',')]
    select: Option<Vec<String>>,

    /// Ignore specific rules (comma-separated)
    #[arg(long, value_delimiter = ',')]
    ignore: Option<Vec<String>>,

    /// Treat warnings as errors
    #[arg(long)]
    warnings_as_errors: bool,

    /// List every rule the binary recognises and exit
    #[arg(long)]
    list_rules: bool,
}

#[derive(clap::ValueEnum, Clone, Debug, Default)]
enum OutputFormat {
    #[default]
    Default,
    Json,
    Compact,
}

/// Subcommands
#[derive(Subcommand, Debug)]
enum Commands {
    /// Show documentation for a specific rule
    Rule {
        /// The rule ID (e.g., R001)
        rule_id: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli) {
        Ok(exit_code) => exit_code,
        Err(e) => {
            // Use the single-line sanitizer because the error chain
            // can embed a hostile filename (e.g. via
            // `git_error_msg(format!("File '{}'...", path.display()))`)
            // and a `\n` in the path would otherwise inject fake
            // error lines on stderr.
            eprintln!(
                "{}: {}",
                "error".red().bold(),
                sanitize_text(&e.to_string(), SanitizePolicy::SingleLine)
            );
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // Handle subcommands
    if let Some(command) = cli.command {
        return match command {
            Commands::Rule { rule_id } => run_rule_command(&rule_id),
        };
    }

    // `--list-rules` short-circuits before anything else: no config
    // load, no file discovery, just print the catalogue and exit.
    if cli.list_rules {
        return list_rules();
    }

    // Build config from CLI args. `apply_cli_overrides` treats `--ignore`
    // as additive to the file's ignore list and `--select` as replacing it.
    let mut config = load_config()?;
    config.apply_cli_overrides(cli.select, cli.ignore, cli.warnings_as_errors);

    let diff_mode = match (cli.diff.as_deref(), cli.diff_staged.as_deref()) {
        (Some(base_ref), None) => Some(DiffMode::Head { base_ref }),
        (None, Some(base_ref)) => Some(DiffMode::Staged { base_ref }),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap prevents --diff and --diff-staged together"),
    };

    // Discover migration files (with exclude patterns from config)
    let migration_paths = discover_migrations(&cli.paths, diff_mode, &config.exclude)?;

    // If no migrations found, that's OK
    if migration_paths.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    // Parse and analyze migrations
    let mut all_diagnostics = Vec::new();
    let mut migrations: Vec<Migration> = Vec::new();
    let mut has_parse_errors = false;

    let rule_registry = RuleRegistry::new();

    for path in &migration_paths {
        match parse_and_check_file(path, &rule_registry, &config, diff_mode) {
            Ok((migration, diagnostics)) => {
                all_diagnostics.extend(diagnostics);
                migrations.push(migration);
            }
            Err(e) => {
                eprintln!(
                    "{}: {} - {}",
                    "error".red().bold(),
                    sanitize_path(path),
                    sanitize_text(&e.to_string(), SanitizePolicy::SingleLine)
                );
                has_parse_errors = true;
            }
        }
    }

    // Run changeset rules if in diff mode
    if let Some(diff_mode) = diff_mode {
        let other_files = discover_non_migration_files(diff_mode)?;
        let changeset_registry = ChangesetRuleRegistry::new();
        let migration_refs: Vec<&Migration> = migrations.iter().collect();
        let other_file_refs: Vec<&Path> = other_files.iter().map(|p| p.as_path()).collect();

        let changeset_diagnostics =
            changeset_registry.check(&migration_refs, &other_file_refs, &config);
        all_diagnostics.extend(changeset_diagnostics);
    }

    // Sort diagnostics by (path, line, column) so output reads in source
    // order rather than rule-iteration order. Rules iterate file-by-file
    // and rule-by-rule, which would otherwise group all R001s together
    // before all R002s for the same file, etc. — the opposite of what
    // most linters do.
    all_diagnostics.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.span.start_line.cmp(&b.span.start_line))
            .then(a.span.start_column.cmp(&b.span.start_column))
            .then(a.rule_id.cmp(b.rule_id))
    });

    // Output results
    output_diagnostics(&all_diagnostics, &cli.output_format);

    // Determine exit code
    // Exit 2 for parse errors (tool error)
    if has_parse_errors {
        return Ok(ExitCode::from(2));
    }

    // Exit 1 for lint errors
    let has_errors = all_diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error);
    if has_errors {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Print every rule the binary recognises, sorted by ID, with its
/// name and severity. One row per rule, columns separated by two
/// spaces — easy to grep and reasonable to read.
fn list_rules() -> Result<ExitCode> {
    let registry = RuleRegistry::new();
    let changeset_registry = ChangesetRuleRegistry::new();

    let mut rows: Vec<(String, String, String)> = registry
        .rules()
        .iter()
        .map(|r| {
            (
                r.id().to_string(),
                r.name().to_string(),
                format!("{:?}", r.severity()),
            )
        })
        .chain(changeset_registry.rules().iter().map(|r| {
            (
                r.id().to_string(),
                r.name().to_string(),
                format!("{:?}", r.severity()),
            )
        }))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.dedup_by(|a, b| a.0 == b.0);

    // Width the ID and severity columns so the name column lines up.
    let id_width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4);
    let sev_width = rows.iter().map(|r| r.2.len()).max().unwrap_or(7);

    for (id, name, sev) in &rows {
        println!(
            "{:<id_width$}  {:<sev_width$}  {}",
            id.bold().cyan(),
            sev,
            name,
            id_width = id_width,
            sev_width = sev_width,
        );
    }

    Ok(ExitCode::SUCCESS)
}

fn run_rule_command(rule_id: &str) -> Result<ExitCode> {
    let registry = RuleRegistry::new();
    let changeset_registry = ChangesetRuleRegistry::new();

    // Check per-file rules
    if let Some(rule) = registry.get(rule_id) {
        println!("{}", rule_id.bold().cyan());
        println!("{}: {}", "Name".bold(), rule.name());
        println!("{}: {:?}", "Severity".bold(), rule.severity());
        println!();
        println!("{}", rule.description());
        return Ok(ExitCode::SUCCESS);
    }

    // Check changeset rules
    if let Some(rule) = changeset_registry.get(rule_id) {
        println!("{}", rule_id.bold().cyan());
        println!("{}: {}", "Name".bold(), rule.name());
        println!("{}: {:?}", "Severity".bold(), rule.severity());
        println!();
        println!("{}", rule.description());
        return Ok(ExitCode::SUCCESS);
    }

    // No `eprintln!` here: returning the error propagates to `main()`,
    // which renders it through the standard sanitized error path. The
    // previous explicit print produced a duplicate "error: Unknown
    // rule: X" line. The error carries the sorted list of valid rule
    // IDs so the user gets actionable feedback instead of having to
    // grep the docs.
    let mut available: Vec<String> = registry
        .rules()
        .iter()
        .map(|r| r.id().to_string())
        .chain(
            changeset_registry
                .rules()
                .iter()
                .map(|r| r.id().to_string()),
        )
        .collect();
    available.sort();
    available.dedup();
    Err(Error::unknown_rule(rule_id, available))
}

fn load_config() -> Result<Config> {
    // Load config from current directory (handles precedence automatically)
    let current_dir = std::env::current_dir().map_err(|e| Error::io(e, PathBuf::from(".")))?;
    Config::load_from_directory(&current_dir)
}

#[derive(Clone, Copy)]
enum DiffMode<'a> {
    Head { base_ref: &'a str },
    Staged { base_ref: &'a str },
}

fn discover_migrations(
    paths: &[PathBuf],
    diff_mode: Option<DiffMode<'_>>,
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    if let Some(diff_mode) = diff_mode {
        // In diff mode, get changed migrations from git
        let repo = GitRepo::open(Path::new("."))?;
        let migrations = match diff_mode {
            DiffMode::Head { base_ref } => {
                repo.changed_paths(base_ref, DiffSource::Head, ChangedKind::Migrations)?
            }
            DiffMode::Staged { base_ref } => {
                repo.changed_paths(base_ref, DiffSource::Index, ChangedKind::Migrations)?
            }
        };

        // Apply exclude patterns to diff mode as well
        if exclude_patterns.is_empty() {
            Ok(migrations)
        } else {
            let patterns = compile_glob_patterns(exclude_patterns)?;
            Ok(migrations
                .into_iter()
                .filter(|p| {
                    let path_str = p.to_string_lossy();
                    !patterns.iter().any(|pat| pat.matches(&path_str))
                })
                .collect())
        }
    } else {
        // In normal mode, discover migrations in paths
        // For explicitly passed files, accept any .py file
        // For directories, use the migration pattern discovery
        let mut all_migrations = Vec::new();

        let patterns = compile_glob_patterns(exclude_patterns)?;

        for path in paths {
            if !path.exists() {
                return Err(Error::path_not_found(path.clone()));
            }

            // `path.is_file()` follows symlinks, so an
            // explicitly-passed symlink to e.g. `/etc/passwd`
            // would otherwise reach the parser. Use
            // `symlink_metadata` to stat the link itself — the
            // CLI's symlink-rejection policy is uniform across
            // the discovery walk, the explicit-path branch
            // here, and the parser's size check.
            let is_regular_file = std::fs::symlink_metadata(path)
                .map(|m| m.file_type().is_file())
                .unwrap_or(false);
            if is_regular_file {
                // Accept any .py file passed explicitly
                if path.extension().is_some_and(|ext| ext == "py") {
                    // Check against exclude patterns
                    let path_str = path.to_string_lossy();
                    if !patterns.iter().any(|pat| pat.matches(&path_str)) {
                        all_migrations.push(path.clone());
                    }
                }
            } else {
                // For directories, use pattern-based discovery with exclude
                let migrations = discovery::discover_migrations_with_exclude(
                    std::slice::from_ref(path),
                    exclude_patterns,
                )?;
                all_migrations.extend(migrations);
            }
        }

        Ok(all_migrations)
    }
}

fn compile_glob_patterns(patterns: &[String]) -> Result<Vec<glob::Pattern>> {
    patterns
        .iter()
        .map(|p| {
            glob::Pattern::new(p).map_err(|e| Error::InvalidGlobPattern {
                pattern: p.clone(),
                message: e.to_string(),
            })
        })
        .collect()
}

/// Returns repo-relative paths so changeset rules can match without
/// touching the filesystem.
fn discover_non_migration_files(diff_mode: DiffMode<'_>) -> Result<Vec<PathBuf>> {
    let repo = GitRepo::open(Path::new("."))?;
    let root = repo.root()?;
    let absolute = match diff_mode {
        DiffMode::Head { base_ref } => {
            repo.changed_paths(base_ref, DiffSource::Head, ChangedKind::NonMigrations)?
        }
        DiffMode::Staged { base_ref } => {
            repo.changed_paths(base_ref, DiffSource::Index, ChangedKind::NonMigrations)?
        }
    };
    Ok(absolute
        .into_iter()
        .map(|p| p.strip_prefix(&root).map(|r| r.to_path_buf()).unwrap_or(p))
        .collect())
}

fn parse_and_check_file(
    path: &Path,
    rule_registry: &RuleRegistry,
    config: &Config,
    diff_mode: Option<DiffMode<'_>>,
) -> Result<(Migration, Vec<Diagnostic>)> {
    // Migration::from_source / from_path bundle size check +
    // parse + extract, returning a path-bearing error on syntax
    // failure either way. Staged content comes from the git
    // index blob (which has its own MAX_FILE_SIZE enforced by
    // GitRepo); disk content goes through parse_file.
    let migration = match diff_mode {
        Some(DiffMode::Staged { .. }) => {
            let repo = GitRepo::open(Path::new("."))?;
            let source = repo.read_staged_file(path)?;
            Migration::from_source(path, &source)?
        }
        _ => Migration::from_path(path)?,
    };

    // Run rules
    let diagnostics = rule_registry.check(&migration, config);

    Ok((migration, diagnostics))
}

fn output_diagnostics(diagnostics: &[Diagnostic], format: &OutputFormat) {
    if diagnostics.is_empty() {
        if matches!(format, OutputFormat::Json) {
            output_json(diagnostics);
        }
        return;
    }

    match format {
        OutputFormat::Default => output_default(diagnostics),
        OutputFormat::Json => output_json(diagnostics),
        OutputFormat::Compact => output_compact(diagnostics),
    }
}

/// Controls whether literal newlines are preserved while escaping
/// terminal control characters for human-readable output.
#[derive(Debug, Clone, Copy)]
enum SanitizePolicy {
    Multiline,
    SingleLine,
}

fn sanitize_text(s: &str, policy: SanitizePolicy) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\n' && matches!(policy, SanitizePolicy::Multiline) {
            out.push(ch);
        } else if (ch as u32) < 0x20 || ch == '\x7f' {
            out.push_str(&format!("\\x{:02x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

fn sanitize_path(path: &std::path::Path) -> String {
    sanitize_text(&path.display().to_string(), SanitizePolicy::SingleLine)
}

fn output_default(diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let severity_str = match diag.severity {
            Severity::Error => "error".red().bold(),
            Severity::Warning => "warning".yellow().bold(),
        };

        println!(
            "{}: {} [{} {}]",
            severity_str,
            sanitize_text(&diag.message, SanitizePolicy::SingleLine),
            diag.rule_id.cyan(),
            diag.rule_name.cyan(),
        );
        println!(
            "  {} {}:{}",
            "-->".blue(),
            sanitize_path(&diag.path),
            diag.span.start_line
        );

        if let Some(ref help) = diag.help {
            println!(
                "  {} {}",
                "help:".green(),
                sanitize_text(help, SanitizePolicy::Multiline)
            );
        }

        println!();
    }

    // Summary
    let error_count = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();

    if error_count > 0 || warning_count > 0 {
        let mut parts = Vec::new();
        if error_count > 0 {
            parts.push(pluralize(error_count, "error", "errors").red().to_string());
        }
        if warning_count > 0 {
            parts.push(
                pluralize(warning_count, "warning", "warnings")
                    .yellow()
                    .to_string(),
            );
        }
        println!("{}", parts.join(", "));
    }
}

fn pluralize(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", n, if n == 1 { one } else { many })
}

#[derive(serde::Serialize)]
struct JsonDiagnostic {
    rule_id: String,
    rule_name: String,
    message: String,
    severity: String,
    path: String,
    line: usize,
    column: usize,
    help: Option<String>,
}

#[derive(serde::Serialize)]
struct JsonOutput {
    diagnostics: Vec<JsonDiagnostic>,
    summary: JsonSummary,
}

#[derive(serde::Serialize)]
struct JsonSummary {
    total: usize,
    errors: usize,
    warnings: usize,
}

fn output_json(diagnostics: &[Diagnostic]) {
    let json_diagnostics: Vec<JsonDiagnostic> = diagnostics
        .iter()
        .map(|d| JsonDiagnostic {
            rule_id: d.rule_id.to_string(),
            rule_name: d.rule_name.to_string(),
            message: d.message.clone(),
            severity: format!("{:?}", d.severity).to_lowercase(),
            path: d.path.display().to_string(),
            line: d.span.start_line,
            column: d.span.start_column,
            help: d.help.clone(),
        })
        .collect();

    let output = JsonOutput {
        diagnostics: json_diagnostics,
        summary: JsonSummary {
            total: diagnostics.len(),
            errors: diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .count(),
            warnings: diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Warning)
                .count(),
        },
    };

    match serde_json::to_string_pretty(&output) {
        Ok(json) => println!("{}", json),
        Err(e) => eprintln!(
            "{}: Failed to serialize JSON output: {}",
            "error".red().bold(),
            e
        ),
    }
}

fn output_compact(diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let severity_char = match diag.severity {
            Severity::Error => "E",
            Severity::Warning => "W",
        };
        println!(
            "{}:{}: {}: [{} {}] {}",
            sanitize_path(&diag.path),
            diag.span.start_line,
            severity_char,
            diag.rule_id,
            diag.rule_name,
            sanitize_text(&diag.message, SanitizePolicy::SingleLine),
        );
        if let Some(help) = &diag.help {
            println!("  help: {}", sanitize_text(help, SanitizePolicy::Multiline));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_text, SanitizePolicy};

    #[test]
    fn sanitize_multiline_preserves_newlines_and_escapes_controls() {
        let cases = [
            ("hello world", "hello world"),
            ("a\nb\tc", "a\nb\\x09c"),
            ("evil\x1b[31mRED\x1b[0m", "evil\\x1b[31mRED\\x1b[0m"),
            ("\rfoo", "\\x0dfoo"),
            ("bar\x07", "bar\\x07"),
            ("a\x7fb", "a\\x7fb"),
            ("café — naïve", "café — naïve"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_text(input, SanitizePolicy::Multiline), expected);
        }
    }

    #[test]
    fn sanitize_single_line_escapes_newlines_and_carriage_returns() {
        let cases = [
            (
                "foo\n  --> /etc/passwd:1\nbar",
                "foo\\x0a  --> /etc/passwd:1\\x0abar",
            ),
            ("a\rb", "a\\x0db"),
            (
                "/repo/app/migrations/0001.py",
                "/repo/app/migrations/0001.py",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_text(input, SanitizePolicy::SingleLine), expected);
        }
    }
}
