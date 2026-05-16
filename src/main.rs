//! Zero-Downtime Migrations CLI
//!
//! A PostgreSQL migration safety linter for Django.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colored::Colorize;

use zero_downtime_migrations::ast::extractor::MigrationExtractor;
use zero_downtime_migrations::ast::Migration;
use zero_downtime_migrations::config::Config;
use zero_downtime_migrations::diagnostics::{Diagnostic, Severity};
use zero_downtime_migrations::discovery;
use zero_downtime_migrations::error::{Error, Result};
use zero_downtime_migrations::git::GitRepo;
use zero_downtime_migrations::parser::{self, ParsedMigration};
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
            // Sanitize because the error chain can embed a hostile
            // filename (e.g. via `git_error_msg(format!("File '{}'...
            // ", path.display()))`).
            eprintln!(
                "{}: {}",
                "error".red().bold(),
                sanitize_for_terminal(&e.to_string())
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
                    sanitize_for_terminal(&e.to_string())
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
    // rule: X" line.
    Err(Error::unknown_rule(rule_id))
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
            DiffMode::Head { base_ref } => repo.changed_migration_paths(base_ref)?,
            DiffMode::Staged { base_ref } => repo.changed_staged_migration_paths(base_ref)?,
        };

        // Apply exclude patterns to diff mode as well
        if exclude_patterns.is_empty() {
            Ok(migrations)
        } else {
            let patterns: Vec<glob::Pattern> = exclude_patterns
                .iter()
                .map(|p| {
                    glob::Pattern::new(p).expect("exclude patterns are validated at config load")
                })
                .collect();
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

        // Compile exclude patterns once
        let patterns: Vec<glob::Pattern> = exclude_patterns
            .iter()
            .map(|p| glob::Pattern::new(p).expect("exclude patterns are validated at config load"))
            .collect();

        for path in paths {
            if !path.exists() {
                return Err(Error::path_not_found(path.clone()));
            }

            if path.is_file() {
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

/// Returns repo-relative paths so changeset rules can match without
/// touching the filesystem.
fn discover_non_migration_files(diff_mode: DiffMode<'_>) -> Result<Vec<PathBuf>> {
    let repo = GitRepo::open(Path::new("."))?;
    let root = repo.root()?;
    let absolute = match diff_mode {
        DiffMode::Head { base_ref } => repo.changed_non_migration_paths(base_ref)?,
        DiffMode::Staged { base_ref } => repo.changed_staged_non_migration_paths(base_ref)?,
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
    // Both paths enforce a size cap (parser::MAX_FILE_SIZE) to bound memory
    // and parse time on untrusted input. The regular path uses parse_file,
    // which also reports syntax errors with line/column. The staged path
    // emits a less precise error on syntax failure; aligning the two would
    // mean exposing parse_file's internals as a source-string variant —
    // tracked as a follow-up.
    let parsed = match diff_mode {
        Some(DiffMode::Staged { .. }) => {
            let repo = GitRepo::open(Path::new("."))?;
            let source = repo.read_staged_file(path)?;
            parser::check_size(path, source.len() as u64)?;
            let parsed = ParsedMigration::parse(&source)
                .map_err(|e| Error::parse(path.to_path_buf(), e.to_string()))?;
            if parsed.has_errors() {
                return Err(Error::parse(
                    path.to_path_buf(),
                    "syntax error in migration file".to_string(),
                ));
            }
            parsed
        }
        _ => ParsedMigration::parse_file(path)?,
    };

    let extractor = MigrationExtractor::new(&parsed);
    let migration = extractor
        .extract(path)
        .map_err(|e| Error::parse(path.to_path_buf(), e.to_string()))?;

    // Run rules
    let diagnostics = rule_registry.check(&migration, config);

    Ok((migration, diagnostics))
}

fn output_diagnostics(diagnostics: &[Diagnostic], format: &OutputFormat) {
    if diagnostics.is_empty() {
        return;
    }

    match format {
        OutputFormat::Default => output_default(diagnostics),
        OutputFormat::Json => output_json(diagnostics),
        OutputFormat::Compact => output_compact(diagnostics),
    }
}

/// Escape ASCII control characters so a migration that smuggles
/// ANSI sequences, carriage returns, BEL, or backspace through an
/// identifier (or a filename) can't repaint the user's terminal when
/// the diagnostic renders. Newlines are passed through because the
/// help text uses them for multi-line layout; tabs are escaped so a
/// `\t`-heavy message can't desync the compact format's
/// `path:line:severity:...` column layout. Everything else in
/// 0x00–0x1F and 0x7F becomes `\xHH`. JSON output is already safe
/// because `serde_json` escapes control characters by default.
///
/// Out of scope: dangerous Unicode codepoints (bidi-override U+202E,
/// zero-width joiner, line/paragraph separators U+2028/U+2029). Rule
/// messages legitimately contain em-dashes and smart quotes, so a
/// blanket non-ASCII escape would mangle them. A future pass can add
/// a narrower deny-list for the known-bad codepoints.
fn sanitize_for_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\n' {
            out.push(ch);
        } else if (ch as u32) < 0x20 || ch == '\x7f' {
            out.push_str(&format!("\\x{:02x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

/// Render a path for terminal display, escaping control characters
/// in case the filename itself is hostile (Unix allows ESC, BEL, CR,
/// etc. in filenames).
fn sanitize_path(path: &std::path::Path) -> String {
    sanitize_for_terminal(&path.display().to_string())
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
            sanitize_for_terminal(&diag.message),
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
            println!("  {} {}", "help:".green(), sanitize_for_terminal(help));
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
            parts.push(
                format!(
                    "{} {}",
                    error_count,
                    if error_count == 1 { "error" } else { "errors" }
                )
                .red()
                .to_string(),
            );
        }
        if warning_count > 0 {
            parts.push(
                format!(
                    "{} {}",
                    warning_count,
                    if warning_count == 1 {
                        "warning"
                    } else {
                        "warnings"
                    }
                )
                .yellow()
                .to_string(),
            );
        }
        println!("{}", parts.join(", "));
    }
}

fn output_json(diagnostics: &[Diagnostic]) {
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
            sanitize_for_terminal(&diag.message),
        );
        if let Some(help) = &diag.help {
            println!("  help: {}", sanitize_for_terminal(help));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_for_terminal;

    #[test]
    fn sanitize_passes_through_printable_ascii() {
        assert_eq!(sanitize_for_terminal("hello world"), "hello world");
    }

    #[test]
    fn sanitize_preserves_newlines_but_escapes_tabs() {
        // Newlines are used by multi-line help text, so they pass
        // through. Tabs would desync the compact format's
        // `path:line:severity:[rule] message` column layout, so they
        // are escaped.
        assert_eq!(sanitize_for_terminal("a\nb\tc"), "a\nb\\x09c");
    }

    #[test]
    fn sanitize_escapes_backspace() {
        // \x08 can erase preceding chars on most terminals, hiding
        // arbitrary content from a diagnostic.
        assert_eq!(sanitize_for_terminal("foo\x08\x08bar"), "foo\\x08\\x08bar");
    }

    #[test]
    fn sanitize_escapes_ansi_color_injection() {
        // A migration whose model name is `evil\x1b[31mRED\x1b[0m`
        // would otherwise repaint the terminal when R005 renders
        // "RemoveField 'evil...RED...'".
        let attack = "evil\x1b[31mRED\x1b[0m";
        let out = sanitize_for_terminal(attack);
        assert_eq!(out, "evil\\x1b[31mRED\\x1b[0m");
    }

    #[test]
    fn sanitize_escapes_carriage_return_and_bel() {
        // \r alone is a common trick to overwrite the current line;
        // \x07 rings the terminal bell.
        assert_eq!(sanitize_for_terminal("\rfoo"), "\\x0dfoo");
        assert_eq!(sanitize_for_terminal("bar\x07"), "bar\\x07");
    }

    #[test]
    fn sanitize_escapes_del() {
        assert_eq!(sanitize_for_terminal("a\x7fb"), "a\\x7fb");
    }

    #[test]
    fn sanitize_passes_through_unicode() {
        // Non-ASCII printable codepoints are not terminal control
        // sequences and should not be escaped — the rule messages
        // already use em-dashes and smart quotes.
        assert_eq!(sanitize_for_terminal("café — naïve"), "café — naïve");
    }
}
