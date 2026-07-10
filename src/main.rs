//! Zero-Downtime Migrations CLI
//!
//! A PostgreSQL migration safety linter for Django.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colored::Colorize;

use zero_downtime_migrations::ast::Migration;
use zero_downtime_migrations::config::Config;
use zero_downtime_migrations::diagnostics::{Diagnostic, Severity};
use zero_downtime_migrations::discovery;
use zero_downtime_migrations::error::{Error, Result};
use zero_downtime_migrations::git::{ChangedFile, ChangedKind, DiffSource, GitRepo};
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
        if cli.paths != [PathBuf::from(".")]
            || cli.diff.is_some()
            || cli.diff_staged.is_some()
            || !matches!(cli.output_format, OutputFormat::Default)
            || cli.select.is_some()
            || cli.ignore.is_some()
            || cli.warnings_as_errors
        {
            return Err(Error::cli_usage(
                "--list-rules cannot be combined with paths or linting flags",
            ));
        }
        return list_rules();
    }

    // Build config from CLI args. `apply_cli_overrides` treats `--ignore`
    // as additive to the file's ignore list and `--select` as replacing it.
    let mut config = load_config()?;
    config.apply_cli_overrides(cli.select, cli.ignore, cli.warnings_as_errors);
    validate_config_rule_ids(&config)?;

    let diff_mode = match (cli.diff.as_deref(), cli.diff_staged.as_deref()) {
        (Some(base_ref), None) => Some(DiffMode::Head { base_ref }),
        (None, Some(base_ref)) => Some(DiffMode::Staged { base_ref }),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap prevents --diff and --diff-staged together"),
    };
    let diff = diff_mode.map(DiffContext::new).transpose()?;

    // Discover migration files (with exclude patterns from config)
    let migration_paths = discover_migrations(&cli.paths, diff.as_ref(), &config.exclude)?;
    let changed_migration_paths = match diff.as_ref() {
        Some(diff) => discover_migration_changes(diff, &cli.paths, &config.exclude)?,
        None => Vec::new(),
    };

    // If no migrations found, that's OK
    if migration_paths.is_empty() && changed_migration_paths.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    // Parse and analyze migrations
    let mut all_diagnostics = Vec::new();
    let mut migrations: Vec<Migration> = Vec::new();
    let mut has_parse_errors = false;

    let rule_registry = RuleRegistry::new();

    for path in &migration_paths {
        match parse_and_check_file(path, &rule_registry, &config, diff.as_ref()) {
            Ok((migration, diagnostics)) => {
                all_diagnostics.extend(diagnostics);
                if diff.is_some() {
                    migrations.push(migration);
                }
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
    if let Some(diff) = diff.as_ref() {
        let other_files = discover_non_migration_files(diff, &cli.paths, &config.exclude)?;
        let changeset_registry = ChangesetRuleRegistry::new();
        let migration_refs: Vec<&Migration> = migrations.iter().collect();
        let migration_path_refs: Vec<&Path> = changed_migration_paths
            .iter()
            .map(PathBuf::as_path)
            .collect();
        let other_file_refs: Vec<&Path> = other_files.iter().map(|p| p.as_path()).collect();

        let changeset_diagnostics = changeset_registry.check(
            &migration_refs,
            &migration_path_refs,
            &other_file_refs,
            &config,
        );
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

/// Every (id, name, severity, description) tuple across both
/// registries. Deduplicated and sorted by ID so the rule
/// catalogue is stable across invocations. Used by both
/// `--list-rules` and `rule <id>` (and the latter's "unknown
/// rule: did you mean…" suggestion list), so the three call
/// sites can't drift apart.
fn all_rule_metadata() -> Vec<(String, String, Severity, String)> {
    let registry = RuleRegistry::new();
    let changeset_registry = ChangesetRuleRegistry::new();
    let mut rows: Vec<(String, String, Severity, String)> = registry
        .rules()
        .iter()
        .map(|r| {
            (
                r.id().to_string(),
                r.name().to_string(),
                r.severity(),
                r.description().to_string(),
            )
        })
        .chain(changeset_registry.rules().iter().map(|r| {
            (
                r.id().to_string(),
                r.name().to_string(),
                r.severity(),
                r.description().to_string(),
            )
        }))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.dedup_by(|a, b| a.0 == b.0);
    rows
}

fn validate_config_rule_ids(config: &Config) -> Result<()> {
    let available: Vec<String> = all_rule_metadata().into_iter().map(|(id, ..)| id).collect();
    let known: BTreeSet<&str> = available.iter().map(String::as_str).collect();

    for rule_id in config.select.iter().chain(&config.ignore) {
        if !known.contains(rule_id.as_str()) {
            return Err(Error::unknown_rule(rule_id, available));
        }
    }

    Ok(())
}

/// Print every rule the binary recognises, sorted by ID, with
/// its name and severity. One row per rule.
fn list_rules() -> Result<ExitCode> {
    let rows = all_rule_metadata();
    let id_width = rows.iter().map(|(id, ..)| id.len()).max().unwrap_or(4);
    let sev_width = rows
        .iter()
        .map(|(_, _, sev, _)| sev.label().len())
        .max()
        .unwrap_or(7);

    for (id, name, sev, _) in &rows {
        println!(
            "{:<id_width$}  {:<sev_width$}  {}",
            id.bold().cyan(),
            sev.label(),
            name,
            id_width = id_width,
            sev_width = sev_width,
        );
    }

    Ok(ExitCode::SUCCESS)
}

fn run_rule_command(rule_id: &str) -> Result<ExitCode> {
    let rows = all_rule_metadata();
    if let Some((id, name, severity, description)) = rows.iter().find(|(id, ..)| id == rule_id) {
        println!("{}", id.bold().cyan());
        println!("{}: {}", "Name".bold(), name);
        println!("{}: {}", "Severity".bold(), severity.label());
        println!();
        println!("{}", description);
        return Ok(ExitCode::SUCCESS);
    }

    // No `eprintln!` here: returning the error propagates to `main()`,
    // which renders it through the standard sanitized error path. The
    // previous explicit print produced a duplicate "error: Unknown
    // rule: X" line. The error carries the sorted list of valid rule
    // IDs so the user gets actionable feedback instead of having to
    // grep the docs.
    let available: Vec<String> = rows.into_iter().map(|(id, ..)| id).collect();
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

impl<'a> DiffMode<'a> {
    /// The git diff source this mode reads from.
    fn source(self) -> DiffSource {
        match self {
            DiffMode::Head { .. } => DiffSource::Head,
            DiffMode::Staged { .. } => DiffSource::Index,
        }
    }

    /// The base reference being compared against.
    fn base_ref(self) -> &'a str {
        match self {
            DiffMode::Head { base_ref } | DiffMode::Staged { base_ref } => base_ref,
        }
    }
}

struct DiffContext {
    repo: GitRepo,
    root: PathBuf,
    files: Vec<ChangedFile>,
    source: DiffSource,
}

impl DiffContext {
    fn new(mode: DiffMode<'_>) -> Result<Self> {
        let repo = GitRepo::open(Path::new("."))?;
        let root = repo.root()?;
        let source = mode.source();
        let files = repo.changed_files_for_source(mode.base_ref(), source)?;
        Ok(Self {
            repo,
            root,
            files,
            source,
        })
    }

    fn read_file(&self, path: &Path) -> Result<String> {
        match self.source {
            DiffSource::Head => self.repo.read_head_file(path),
            DiffSource::Index => self.repo.read_staged_file(path),
        }
    }
}

fn discover_migrations(
    paths: &[PathBuf],
    diff: Option<&DiffContext>,
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    if let Some(diff) = diff {
        let migrations = diff.repo.paths_from(&diff.files, ChangedKind::Migrations)?;
        let migrations = filter_paths_by_cli_scope(migrations, paths, &diff.root)?;

        // Apply exclude patterns to diff mode as well
        if exclude_patterns.is_empty() {
            Ok(migrations)
        } else {
            let patterns = compile_glob_patterns(exclude_patterns)?;
            Ok(migrations
                .into_iter()
                .filter(|p| {
                    !discovery::path_matches_any_glob(p, &[Path::new("."), &diff.root], &patterns)
                })
                .collect())
        }
    } else {
        // In normal mode, discover migrations in paths
        // For explicitly passed files, accept any .py file
        // For directories, use the migration pattern discovery
        let mut all_migrations = Vec::new();

        let patterns = compile_glob_patterns(exclude_patterns)?;
        let current_dir = std::env::current_dir().map_err(|e| Error::io(e, PathBuf::from(".")))?;
        let normalized_current_dir = current_dir.canonicalize().ok();

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
            let file_type = std::fs::symlink_metadata(path).map(|m| m.file_type());
            let is_regular_file = file_type.as_ref().map(|t| t.is_file()).unwrap_or(false);
            let is_directory = file_type.as_ref().map(|t| t.is_dir()).unwrap_or(false);
            if is_regular_file {
                // Accept any .py file passed explicitly
                if path.extension().is_some_and(|ext| ext == "py") {
                    // Check against exclude patterns
                    let mut roots = vec![Path::new("."), current_dir.as_path()];
                    if let Some(normalized) = &normalized_current_dir {
                        roots.push(normalized.as_path());
                    }
                    let excluded = discovery::path_matches_any_glob(path, &roots, &patterns)
                        || normalize_existing_path(path)
                            .ok()
                            .is_some_and(|normalized| {
                                discovery::path_matches_any_glob(&normalized, &roots, &patterns)
                            });
                    if !excluded {
                        all_migrations.push(path.clone());
                    }
                }
            } else if is_directory {
                // For directories, use pattern-based discovery with exclude
                let migrations = discovery::discover_migrations_with_exclude(
                    std::slice::from_ref(path),
                    exclude_patterns,
                )?;
                all_migrations.extend(migrations);
            } else {
                return Err(Error::path_not_found(path.clone()));
            }
        }

        all_migrations.sort();
        all_migrations.dedup();
        Ok(all_migrations)
    }
}

fn discover_migration_changes(
    diff: &DiffContext,
    paths: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let touched = diff.repo.migration_touches_from(&diff.files)?;
    let touched = filter_paths_by_cli_scope(touched, paths, &diff.root)?;
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    let patterns = compile_glob_patterns(exclude_patterns)?;
    Ok(touched
        .into_iter()
        .filter(|path| {
            !discovery::path_matches_any_glob(path, &[Path::new("."), &diff.root], &patterns)
        })
        .collect())
}

fn filter_paths_by_cli_scope(
    paths: Vec<PathBuf>,
    scopes: &[PathBuf],
    repo_root: &Path,
) -> Result<Vec<PathBuf>> {
    let current_dir = normalize_existing_path(
        &std::env::current_dir().map_err(|e| Error::io(e, PathBuf::from(".")))?,
    )?;
    let original_repo_root = repo_root.to_path_buf();
    let normalized_repo_root = normalize_existing_path(repo_root)?;
    if scopes == [PathBuf::from(".")] && current_dir == normalized_repo_root {
        return Ok(paths);
    }
    let normalized_paths: Vec<PathBuf> = paths
        .iter()
        .map(|path| {
            normalize_changed_path_for_scope(path, &original_repo_root, &normalized_repo_root)
        })
        .collect();
    let absolute_scopes: Vec<PathBuf> = scopes
        .iter()
        .map(|scope| {
            let path = if scope.is_absolute() {
                scope.clone()
            } else {
                current_dir.join(scope)
            };
            let normalized_scope = if path.exists() {
                normalize_existing_path(&path)?
            } else {
                normalize_changed_path_for_scope(&path, &original_repo_root, &normalized_repo_root)
            };
            if !path_is_in_scope(&normalized_scope, &normalized_repo_root) {
                return Err(Error::path_not_found(scope.clone()));
            }
            if !path.exists()
                && !normalized_paths
                    .iter()
                    .any(|changed| path_is_in_scope(changed, &normalized_scope))
            {
                return Err(Error::path_not_found(scope.clone()));
            }
            Ok(normalized_scope)
        })
        .collect::<Result<_>>()?;
    Ok(paths
        .into_iter()
        .zip(normalized_paths)
        .filter_map(|(path, normalized_path)| {
            absolute_scopes
                .iter()
                .any(|scope| path_is_in_scope(&normalized_path, scope))
                .then_some(path)
        })
        .collect())
}

fn path_is_in_scope(path: &Path, scope: &Path) -> bool {
    if scope == Path::new(".") {
        return true;
    }
    path == scope || path.starts_with(scope)
}

fn normalize_existing_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|e| Error::io(e, path.to_path_buf()))
}

fn normalize_changed_path_for_scope(
    path: &Path,
    original_repo_root: &Path,
    normalized_repo_root: &Path,
) -> PathBuf {
    if let Ok(normalized) = normalize_existing_path(path) {
        return normalized;
    }
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(original_repo_root) {
            return normalized_repo_root.join(relative);
        }
        if let Ok(relative) = path.strip_prefix(normalized_repo_root) {
            return normalized_repo_root.join(relative);
        }
        path.to_path_buf()
    } else {
        normalized_repo_root.join(path)
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
fn discover_non_migration_files(
    diff: &DiffContext,
    paths: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let absolute = diff
        .repo
        .paths_from(&diff.files, ChangedKind::NonMigrations)?;
    if absolute.is_empty() {
        return Ok(Vec::new());
    }
    let absolute = filter_paths_by_cli_scope(absolute, paths, &diff.root)?;
    let normalized_root = normalize_existing_path(&diff.root)?;
    let patterns = compile_glob_patterns(exclude_patterns)?;
    Ok(absolute
        .into_iter()
        .filter(|p| {
            !discovery::path_matches_any_glob(
                p,
                &[Path::new("."), &diff.root, &normalized_root],
                &patterns,
            )
        })
        .map(|p| {
            p.strip_prefix(&diff.root)
                .map(|r| r.to_path_buf())
                .or_else(|_| {
                    normalize_changed_path_for_scope(&p, &diff.root, &normalized_root)
                        .strip_prefix(&normalized_root)
                        .map(|r| r.to_path_buf())
                })
                .unwrap_or(p)
        })
        .collect())
}

fn parse_and_check_file(
    path: &Path,
    rule_registry: &RuleRegistry,
    config: &Config,
    diff: Option<&DiffContext>,
) -> Result<(Migration, Vec<Diagnostic>)> {
    // Migration::from_source / from_path bundle size check +
    // parse + extract, returning a path-bearing error on syntax
    // failure either way. Staged content comes from the git
    // index blob (which has its own MAX_FILE_SIZE enforced by
    // GitRepo); disk content goes through parse_file.
    let migration = match diff {
        Some(diff) => {
            let source = diff.read_file(path)?;
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
        } else if ch.is_control() {
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

/// Count diagnostics by severity, returning `(errors, warnings)`.
fn count_by_severity(diagnostics: &[Diagnostic]) -> (usize, usize) {
    let errors = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    (errors, warnings)
}

fn output_default(diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let label = diag.severity.label();
        let severity_str = match diag.severity {
            Severity::Error => label.red().bold(),
            Severity::Warning => label.yellow().bold(),
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
    let (error_count, warning_count) = count_by_severity(diagnostics);

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
            severity: d.severity.label().to_string(),
            path: d.path.display().to_string(),
            line: d.span.start_line,
            column: d.span.start_column,
            help: d.help.clone(),
        })
        .collect();

    let (errors, warnings) = count_by_severity(diagnostics);
    let output = JsonOutput {
        diagnostics: json_diagnostics,
        summary: JsonSummary {
            total: diagnostics.len(),
            errors,
            warnings,
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
    use super::{normalize_changed_path_for_scope, sanitize_text, SanitizePolicy};
    use std::path::Path;

    #[test]
    fn sanitize_multiline_preserves_newlines_and_escapes_controls() {
        let cases = [
            ("hello world", "hello world"),
            ("a\nb\tc", "a\nb\\x09c"),
            ("evil\x1b[31mRED\x1b[0m", "evil\\x1b[31mRED\\x1b[0m"),
            ("\rfoo", "\\x0dfoo"),
            ("bar\x07", "bar\\x07"),
            ("a\x7fb", "a\\x7fb"),
            ("c\u{009b}31m", "c\\x9b31m"),
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

    #[cfg(unix)]
    #[test]
    fn normalize_changed_path_rebuilds_missing_repo_relative_absolute_path_from_normalized_root() {
        let original_root = Path::new("/tmp/project");
        let normalized_root = Path::new("/private/tmp/project");
        let staged_only_path = Path::new("/tmp/project/app/migrations/0001.py");

        assert_eq!(
            normalize_changed_path_for_scope(staged_only_path, original_root, normalized_root),
            Path::new("/private/tmp/project/app/migrations/0001.py"),
        );
    }
}
