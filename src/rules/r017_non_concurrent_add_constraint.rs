//! R017: Non-concurrent AddConstraint
//!
//! Detects `AddConstraint` operations that hold a table lock while
//! building or validating the constraint:
//!
//!   - `CheckConstraint` — validating the predicate against every
//!     existing row, blocking writes for the duration.
//!   - `ExclusionConstraint` — building its enforcement index
//!     non-concurrently, blocking writes for the build.
//!
//! `UniqueConstraint` has the same problem (the index it builds is
//! non-concurrent), but R002 already flags it with specific guidance
//! about `USING INDEX`, so we leave it to R002 to avoid double-firing.
//!
//! `ConstraintType::Unknown` (a constraint class we couldn't classify
//! from the source) is silently skipped to avoid false positives on
//! unrecognised classes.

use std::collections::HashSet;

use crate::ast::{
    sql_tokens, strip_sql_noise, ConstraintType, Migration, OperationData, OperationType,
};
use crate::diagnostics::{Diagnostic, Severity};
use crate::rules::{walk_with_created_models, Rule, RuleContext};

/// Rule that detects constraints that may cause table locks.
pub struct R017NonConcurrentAddConstraint;

impl Rule for R017NonConcurrentAddConstraint {
    fn id(&self) -> &'static str {
        "R017"
    }

    fn name(&self) -> &'static str {
        "non-concurrent-add-constraint"
    }

    fn description(&self) -> &'static str {
        "AddConstraint with a CHECK or EXCLUDE constraint locks the table — CHECK \
         validates every row, EXCLUDE builds its enforcement index non-concurrently. \
         Migrate CHECK via NOT VALID + VALIDATE. EXCLUDE has no fully-online path \
         in stock PostgreSQL — defer it to a low-traffic window."
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, migration: &Migration, ctx: &RuleContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut pending_validation = HashSet::new();
        walk_with_created_models(migration, |op, created| {
            if op.in_autocommit_block {
                pending_validation.clear();
            }
            if migration.framework == crate::discovery::MigrationFramework::Alembic
                && op.op_type == OperationType::ExecuteSql
            {
                if let OperationData::RunSQL(data) = &op.data {
                    let mut same_call_pending = HashSet::new();
                    let sql = strip_sql_noise(&data.sql);
                    let statements: Vec<_> =
                        sql.split(';').filter(|s| !s.trim().is_empty()).collect();
                    for (statement_index, statement) in statements.iter().enumerate() {
                        if matches!(
                            sql_tokens(statement).first().map(String::as_str),
                            Some("COMMIT" | "ROLLBACK")
                        ) {
                            pending_validation.clear();
                            same_call_pending.clear();
                            continue;
                        }
                        let actions = constraint_sql(statement);
                        let single_fresh_table = statement_index == 0 && actions.len() == 1;
                        for action in actions {
                            match action {
                                SqlConstraint::AddCheck {
                                    table,
                                    name,
                                    not_valid,
                                } => {
                                    if single_fresh_table && created.contains_sql_table(&table) {
                                        continue;
                                    }
                                    if not_valid {
                                        same_call_pending.insert((table.clone(), name.clone()));
                                        if !op.in_autocommit_block {
                                            pending_validation.insert((table, name));
                                        }
                                    } else {
                                        diagnostics.push(Diagnostic::new(
                                        self.id(), self.name(), self.severity(),
                                        "Raw SQL adds a CHECK constraint that validates existing rows",
                                        ctx.path.to_path_buf(), op.span,
                                    ).with_help("Add the constraint as NOT VALID and validate it after that transaction commits."));
                                    }
                                }
                                SqlConstraint::Validate { table, name } => {
                                    if same_call_pending.contains(&(table.clone(), name.clone()))
                                        || (!op.in_autocommit_block
                                            && pending_validation.contains(&(table, name)))
                                    {
                                        diagnostics.push(Diagnostic::new(
                                        self.id(), self.name(), self.severity(),
                                        "Constraint is validated before its NOT VALID addition commits",
                                        ctx.path.to_path_buf(), op.span,
                                    ).with_help("Validate after the ADD CONSTRAINT transaction commits; configure Alembic to use separate transactions if both revisions run together."));
                                    }
                                }
                            }
                        }
                    }
                }
                return;
            }
            if op.op_type != OperationType::AddConstraint {
                return;
            }
            let OperationData::Constraint(data) = &op.data else {
                return;
            };
            if created.contains_operation(migration, op) {
                return;
            }
            if migration.framework == crate::discovery::MigrationFramework::Alembic
                && data.not_valid
                && !op.in_autocommit_block
            {
                if let Some(name) = &data.name {
                    let table = op.table_identity.as_ref().map_or_else(
                        || vec![data.model_name.to_ascii_uppercase()],
                        |identity| {
                            let mut parts = identity
                                .schema
                                .iter()
                                .map(|s| s.to_ascii_uppercase())
                                .collect::<Vec<_>>();
                            parts.push(identity.name.to_ascii_uppercase());
                            parts
                        },
                    );
                    pending_validation.insert((table, name.to_ascii_uppercase()));
                }
            }
            if migration.framework.uses_sql_table_identity()
                && data.not_valid
                && matches!(
                    data.constraint_type,
                    ConstraintType::Check | ConstraintType::ForeignKey
                )
            {
                return;
            }

            // Django FKs go through AddField (covered by R006); SQL-returning
            // migration frameworks represent one directly as a constraint. UniqueConstraint
            // is covered by R002 with USING INDEX guidance.
            let (message, help) = match data.constraint_type {
                ConstraintType::Check => (
                    if migration.framework.uses_sql_table_identity() {
                        "Adding a CHECK constraint validates all existing rows".to_string()
                    } else {
                        "AddConstraint with a CHECK constraint validates all rows".to_string()
                    },
                    if migration.framework.uses_sql_table_identity() {
                        "Add the constraint as NOT VALID, commit that transaction, then validate it. A later revision needs its own transaction if both revisions run together.".to_string()
                    } else {
                        include_str!("help/r017_check_constraint.txt").to_string()
                    },
                ),
                ConstraintType::Exclusion => (
                    if migration.framework.uses_sql_table_identity() {
                        "Adding an EXCLUDE constraint builds its index non-concurrently, locking the table".to_string()
                    } else {
                        "AddConstraint with an EXCLUDE constraint builds its index non-concurrently, locking the table".to_string()
                    },
                    if migration.framework.uses_sql_table_identity() {
                        "PostgreSQL has no fully-online EXCLUDE constraint path; create it with a new table or use a low-traffic window.".to_string()
                    } else {
                        include_str!("help/r017_exclusion_constraint.txt").to_string()
                    },
                ),
                ConstraintType::ForeignKey => (
                    "Adding a FOREIGN KEY validates all existing rows".to_string(),
                    "Add the foreign key as NOT VALID, validate it separately, then enforce it after the application is ready.".to_string(),
                ),
                _ => return,
            };

            diagnostics.push(
                Diagnostic::new(
                    self.id(),
                    self.name(),
                    self.severity(),
                    message,
                    ctx.path.to_path_buf(),
                    op.span,
                )
                .with_help(help),
            );
        });

        diagnostics
    }
}

enum SqlConstraint {
    AddCheck {
        table: Vec<String>,
        name: String,
        not_valid: bool,
    },
    Validate {
        table: Vec<String>,
        name: String,
    },
}

fn constraint_sql(statement: &str) -> Vec<SqlConstraint> {
    // ponytail: direct ALTER TABLE only; use a SQL parser if nested statements need linting.
    let segments = top_level_segments(statement);
    let Some(first) = segments.first() else {
        return vec![];
    };
    let Some((alter, rest)) = take_sql_term(first) else {
        return vec![];
    };
    let Some((table_keyword, mut rest)) = take_sql_term(rest) else {
        return vec![];
    };
    if !alter.eq_ignore_ascii_case("ALTER") || !table_keyword.eq_ignore_ascii_case("TABLE") {
        return vec![];
    }
    if let Some((word, after_if)) = take_sql_term(rest) {
        if word.eq_ignore_ascii_case("IF") {
            let Some((exists, after_exists)) = take_sql_term(after_if) else {
                return vec![];
            };
            if !exists.eq_ignore_ascii_case("EXISTS") {
                return vec![];
            }
            rest = after_exists;
        }
    }
    if let Some((word, after_only)) = take_sql_term(rest) {
        if word.eq_ignore_ascii_case("ONLY") {
            rest = after_only;
        }
    }
    let Some((table_name, first_action)) = take_sql_term(rest) else {
        return vec![];
    };
    let table = sql_tokens(table_name);
    if table.is_empty() {
        return vec![];
    }
    segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            let action_sql = if index == 0 { first_action } else { segment };
            let words = sql_tokens(action_sql);
            let name = words.get(2)?.clone();
            if words.get(1).map(String::as_str) != Some("CONSTRAINT") {
                return None;
            }
            match words.first().map(String::as_str) {
                Some("ADD") if words.get(3).map(String::as_str) == Some("CHECK") => {
                    let suffix = check_suffix(action_sql)?;
                    let suffix_words = sql_tokens(suffix);
                    let not_valid = suffix_words.windows(2).any(|pair| pair == ["NOT", "VALID"]);
                    Some(SqlConstraint::AddCheck {
                        table: table.clone(),
                        name,
                        not_valid,
                    })
                }
                Some("VALIDATE") => Some(SqlConstraint::Validate {
                    table: table.clone(),
                    name,
                }),
                _ => None,
            }
        })
        .collect()
}

fn take_sql_term(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let mut quoted = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '"' {
            if quoted && chars.peek().is_some_and(|(_, next)| *next == '"') {
                chars.next();
            } else {
                quoted = !quoted;
            }
        } else if ch.is_whitespace() && !quoted {
            return Some((&input[..index], &input[index..]));
        }
    }
    Some((input, ""))
}

fn top_level_segments(statement: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut depth: usize = 0;
    let mut quoted = false;
    let mut chars = statement.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '"' {
            if quoted && chars.peek().is_some_and(|(_, next)| *next == '"') {
                chars.next();
            } else {
                quoted = !quoted;
            }
        } else if !quoted {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    segments.push(&statement[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
    }
    segments.push(&statement[start..]);
    segments
}

fn check_suffix(segment: &str) -> Option<&str> {
    let mut depth: usize = 0;
    let mut quoted = false;
    let mut end = None;
    let mut chars = segment.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '"' {
            if quoted && chars.peek().is_some_and(|(_, next)| *next == '"') {
                chars.next();
            } else {
                quoted = !quoted;
            }
        } else if !quoted {
            match ch {
                '(' => depth += 1,
                ')' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + 1);
                    }
                }
                _ => {}
            }
        }
    }
    end.map(|index| &segment[index..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    const EXCLUSION_CONSTRAINT_BAD: &str = r#"
from django.db import migrations
from django.contrib.postgres.constraints import ExclusionConstraint


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='booking',
            constraint=ExclusionConstraint(
                name='exclude_overlapping',
                expressions=[('daterange', '&&')],
            ),
        ),
    ]
"#;

    const EXCLUSION_CONSTRAINT_ON_FRESH_MODEL_GOOD: &str = r#"
from django.db import migrations, models
from django.contrib.postgres.constraints import ExclusionConstraint


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Booking',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
                ('daterange', models.DateRangeField()),
            ],
        ),
        migrations.AddConstraint(
            model_name='booking',
            constraint=ExclusionConstraint(
                name='exclude_overlapping',
                expressions=[('daterange', '&&')],
            ),
        ),
    ]
"#;

    const CREATE_MODEL_WITH_CHECK_CONSTRAINT: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('price', models.DecimalField()),
            ],
        ),
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    fn check_migration(source: &str) -> Vec<Diagnostic> {
        crate::rules::test_support::check_rule(&R017NonConcurrentAddConstraint, source)
    }

    #[test]
    fn test_check_constraint_warns() {
        let diagnostics = check_migration(CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    #[test]
    fn test_exclusion_constraint_bad() {
        // ExclusionConstraint builds its enforcement index non-concurrently,
        // holding an ACCESS EXCLUSIVE lock for the build, so R017 must flag it.
        let diagnostics = check_migration(EXCLUSION_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
        assert!(
            diagnostics[0].message.contains("EXCLUDE"),
            "message should mention EXCLUDE, got: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn test_exclusion_constraint_on_fresh_model_is_exempt() {
        // Same CreateModel exemption as CheckConstraint: a freshly
        // created table has no rows yet, so the lock is harmless.
        let diagnostics = check_migration(EXCLUSION_CONSTRAINT_ON_FRESH_MODEL_GOOD);
        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}",
        );
    }

    #[test]
    fn test_create_model_with_check_constraint_exempt() {
        // CheckConstraint on a model created in same migration should be exempt
        let diagnostics = check_migration(CREATE_MODEL_WITH_CHECK_CONSTRAINT);
        assert!(diagnostics.is_empty());
    }

    const ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
        migrations.CreateModel(
            name='Product',
            fields=[
                ('id', models.AutoField(primary_key=True)),
                ('price', models.DecimalField()),
            ],
        ),
    ]
"#;

    #[test]
    fn test_addconstraint_before_createmodel_is_not_exempted() {
        // Order-aware exemption: a CreateModel that runs *after* the
        // AddConstraint cannot retroactively make the AddConstraint safe.
        let diagnostics = check_migration(ADDCONSTRAINT_BEFORE_CREATEMODEL_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const WRAPPED_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.AddConstraint(
                    model_name='product',
                    constraint=models.CheckConstraint(
                        check=models.Q(price__gte=0),
                        name='positive_price',
                    ),
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_wrapped_database_check_constraint_is_flagged() {
        let diagnostics = check_migration(WRAPPED_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const NESTED_SDAS_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.SeparateDatabaseAndState(
                    database_operations=[
                        migrations.AddConstraint(
                            model_name='product',
                            constraint=models.CheckConstraint(
                                check=models.Q(price__gte=0),
                                name='positive_price',
                            ),
                        ),
                    ],
                ),
            ],
        ),
    ]
"#;

    #[test]
    fn test_nested_sdas_database_check_constraint_is_flagged() {
        let diagnostics = check_migration(NESTED_SDAS_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }

    const CUSTOM_CONSTRAINT_NAME_CONTAINS_CHECK_GOOD: &str = r#"
from django.db import migrations


class MyCheckConstraintLikeThing:
    pass


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            model_name='product',
            constraint=MyCheckConstraintLikeThing(),
        ),
    ]
"#;

    #[test]
    fn test_custom_constraint_with_check_in_name_is_not_classified() {
        let diagnostics = check_migration(CUSTOM_CONSTRAINT_NAME_CONTAINS_CHECK_GOOD);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
    }

    const POSITIONAL_CHECK_CONSTRAINT_BAD: &str = r#"
from django.db import migrations, models


class Migration(migrations.Migration):

    operations = [
        migrations.AddConstraint(
            'product',
            models.CheckConstraint(check=models.Q(price__gte=0), name='positive_price'),
        ),
    ]
"#;

    #[test]
    fn test_positional_check_constraint_warns() {
        let diagnostics = check_migration(POSITIONAL_CHECK_CONSTRAINT_BAD);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule_id, "R017");
    }
}
