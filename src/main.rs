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
use zero_downtime_migrations::parser::ParsedMigration;
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
            eprintln!("{}: {}", "error".red().bold(), e);
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

    // Build config from CLI args
    let mut config = load_config()?;

    if let Some(select) = cli.select {
        config.select = select.into_iter().collect();
    }
    if let Some(ignore) = cli.ignore {
        config.ignore = ignore.into_iter().collect();
    }
    if cli.warnings_as_errors {
        config.warnings_as_errors = true;
    }

    // Discover migration files (with exclude patterns from config)
    let migration_paths = discover_migrations(&cli.paths, cli.diff.as_deref(), &config.exclude)?;

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
        match parse_and_check_file(path, &rule_registry, &config) {
            Ok((migration, diagnostics)) => {
                all_diagnostics.extend(diagnostics);
                migrations.push(migration);
            }
            Err(e) => {
                eprintln!("{}: {} - {}", "error".red().bold(), path.display(), e);
                has_parse_errors = true;
            }
        }
    }

    // Run changeset rules if in diff mode
    if cli.diff.is_some() {
        let other_files = discover_non_migration_files(cli.diff.as_deref())?;
        let changeset_registry = ChangesetRuleRegistry::new();
        let migration_refs: Vec<&Migration> = migrations.iter().collect();
        let other_file_refs: Vec<&Path> = other_files.iter().map(|p| p.as_path()).collect();

        let changeset_diagnostics =
            changeset_registry.check(&migration_refs, &other_file_refs, &config);
        all_diagnostics.extend(changeset_diagnostics);
    }

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

    eprintln!("{}: Unknown rule: {}", "error".red().bold(), rule_id);
    Err(Error::unknown_rule(rule_id))
}

fn load_config() -> Result<Config> {
    // Load config from current directory (handles precedence automatically)
    let current_dir = std::env::current_dir().map_err(|e| Error::io(e, PathBuf::from(".")))?;
    Config::load_from_directory(&current_dir)
}

fn discover_migrations(
    paths: &[PathBuf],
    diff_ref: Option<&str>,
    exclude_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    if let Some(base_ref) = diff_ref {
        // In diff mode, get changed migrations from git
        let repo = GitRepo::open(Path::new("."))?;
        let migrations = repo.changed_migration_paths(base_ref)?;

        // Apply exclude patterns to diff mode as well
        if exclude_patterns.is_empty() {
            Ok(migrations)
        } else {
            let patterns: Vec<glob::Pattern> = exclude_patterns
                .iter()
                .filter_map(|p| glob::Pattern::new(p).ok())
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
            .filter_map(|p| glob::Pattern::new(p).ok())
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

fn discover_non_migration_files(diff_ref: Option<&str>) -> Result<Vec<PathBuf>> {
    if let Some(base_ref) = diff_ref {
        let repo = GitRepo::open(Path::new("."))?;
        repo.changed_non_migration_paths(base_ref)
    } else {
        Ok(Vec::new())
    }
}

fn parse_and_check_file(
    path: &Path,
    rule_registry: &RuleRegistry,
    config: &Config,
) -> Result<(Migration, Vec<Diagnostic>)> {
    // Read and parse the file
    let source = std::fs::read_to_string(path).map_err(|e| Error::io(e, path.to_path_buf()))?;

    let parsed = ParsedMigration::parse(&source)
        .map_err(|e| Error::parse(path.to_path_buf(), e.to_string()))?;

    // Check for parse errors in the syntax tree
    if parsed.has_errors() {
        return Err(Error::parse(
            path.to_path_buf(),
            "syntax error in migration file".to_string(),
        ));
    }

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

fn output_default(diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let severity_str = match diag.severity {
            Severity::Error => "error".red().bold(),
            Severity::Warning => "warning".yellow().bold(),
        };

        println!(
            "{}: {} [{}]",
            severity_str,
            diag.message,
            diag.rule_id.cyan()
        );
        println!(
            "  {} {}:{}",
            "-->".blue(),
            diag.path.display(),
            diag.span.start_line
        );

        if let Some(ref help) = diag.help {
            println!("  {} {}", "help:".green(), help);
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
            "{}:{}: {}: {} [{}]",
            diag.path.display(),
            diag.span.start_line,
            severity_char,
            diag.rule_id,
            diag.message
        );
    }
}
