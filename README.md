# zero-downtime-migrations (zdm)

A PostgreSQL migration safety linter for Django.

## Why

Deploying database migrations without downtime requires careful attention to how PostgreSQL acquires locks. Operations like adding an index, altering a column to NOT NULL, or adding a foreign key can lock tables for extended periods on large datasets, blocking reads and writes and causing outages. zdm statically analyzes Django migration files to catch these unsafe patterns before they reach production, helping teams ship schema changes safely during normal deployments.

## What

A standalone Rust CLI tool that statically analyzes Django migration files to catch unsafe patterns that cause table locks, outages, and data loss on large PostgreSQL databases. Distributed like ruff/uv — a single fast binary, installable via `pip`, `uvx`, or standalone download.

**Supports Django 3.2+** — zdm parses migration files directly without importing Django, so it works with any Django version and doesn't require Django to be installed.

## Installation

```bash
# Install via pip
pip install zero-downtime-migrations

# Or use uvx to run without installing
uvx zero-downtime-migrations .

# Or install with pipx
pipx install zero-downtime-migrations
```

## Usage

```bash
# These are equivalent
zero-downtime-migrations app/migrations/0042_add_index.py
zdm app/migrations/0042_add_index.py

# Lint all migrations in a directory
zdm app/migrations/

# Lint all migrations in the project
zdm .

# Diff mode: lint changed migrations in a PR
zdm --diff origin/main

# Output formats
zdm --output-format json .
zdm --output-format compact .

# Select/ignore specific rules
zdm --select R001,R003 .
zdm --ignore R008 .

# Show explanation for a rule
zdm rule R001

# Treat warnings as errors
zdm --warnings-as-errors .
```

### Exit Codes

- `0` — no issues found
- `1` — lint violations found (errors). Warnings alone do NOT cause exit code 1 unless `--warnings-as-errors` is set.
- `2` — tool error (bad arguments, config parse failure, invalid file path)

## Rules

| Rule | Name | Severity | Description |
|------|------|----------|-------------|
| R001 | non-concurrent-add-index | Error | Use `AddIndexConcurrently` instead of `AddIndex` |
| R002 | unique-constraint-without-index | Error | Unique constraints should have a concurrent index |
| R003 | runsql-create-index | Error | Use `AddIndexConcurrently` instead of raw SQL `CREATE INDEX` |
| R004 | missing-atomic-false | Error | Non-atomic migrations require `atomic = False` |
| R005 | remove-field-without-separate | Error | Use `SeparateDatabaseAndState` to remove fields safely |
| R006 | add-field-foreign-key | Warning | Adding FK creates index and validates constraint |
| R007 | fk-without-concurrent-index | Warning | Foreign keys should have a concurrent index |
| R008 | disallowed-file-changes | Warning | Don't change app code alongside migrations |
| R009 | separate-db-state-same-pr | Warning | Don't deploy both steps of `SeparateDatabaseAndState` together |
| R010 | add-field-not-null | Error | Adding NOT NULL field without default rewrites table |
| R011 | rename-field | Warning | Renaming fields can break running code |
| R012 | irreversible-run-python | Warning | `RunPython` should have a reverse function |
| R013 | irreversible-run-sql | Warning | `RunSQL` should have a reverse SQL |
| R014 | model-imports | Error | Don't import models in `RunPython` |
| R015 | alter-field-not-null | Error | Changing field to NOT NULL validates all rows |
| R016 | non-concurrent-remove-index | Error | Use `RemoveIndexConcurrently` instead of `RemoveIndex` |
| R017 | non-concurrent-add-constraint | Warning | Adding CHECK/FK constraints validates all rows |

### CreateModel Exemption

Several rules (R001, R002, R006, R007, R010, R017) automatically exempt operations that target models created in the same migration. This is because operations on newly created (empty) tables don't cause the locking issues these rules detect.

For example, this migration will NOT trigger R001:

```python
class Migration(migrations.Migration):
    operations = [
        migrations.CreateModel(
            name='Order',
            fields=[('id', models.AutoField(primary_key=True))],
        ),
        migrations.AddIndex(  # Exempt: 'order' was just created above
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_idx'),
        ),
    ]
```

### R015 Limitation

R015 (alter-field-not-null) cannot determine whether a field was previously nullable. It flags ALL `AlterField` operations where the resulting field is NOT NULL, which may produce false positives when the field was already NOT NULL. This is a fundamental limitation of static analysis without schema history. Use `# zdm: ignore R015` inline comments for legitimate `AlterField` operations that don't change nullability.

## Configuration

Configure via `pyproject.toml` or `zero-downtime-migrations.toml`:

```toml
[tool.zdm]
select = ["R001", "R002"]
ignore = ["R008"]
warnings-as-errors = false
disallowed-file-patterns = ["*.py"]
exclude = ["**/test_migrations/**"]
```

### Configuration Precedence

Settings are applied in this order (highest to lowest priority):

1. **CLI flags** (`--select`, `--ignore`, `--warnings-as-errors`)
2. **`zero-downtime-migrations.toml`** in the current directory
3. **`pyproject.toml`** `[tool.zdm]` section
4. **Default values**

CLI flags always override config file settings. If both `zero-downtime-migrations.toml` and `pyproject.toml` exist, the standalone file takes precedence.

## Pre-commit Integration

Add to your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/Photoroom/zero-downtime-migrations
    rev: v0.1.0
    hooks:
      - id: zdm
```

Or use diff mode to only check changed migrations:

```yaml
repos:
  - repo: https://github.com/Photoroom/zero-downtime-migrations
    rev: v0.1.0
    hooks:
      - id: zdm-diff
```

## GitHub Actions

```yaml
- name: Install zdm
  run: pip install zero-downtime-migrations

- name: Lint migrations
  run: zdm --diff origin/main
```

## Comparison with Other Tools

| | zdm | django-migration-linter | Django's `makemigrations --check` |
|---|---|---|---|
| **Requires Django installed** | No | Yes | Yes |
| **Requires project setup** | No | Yes (settings.py) | Yes (full environment) |
| **Checks for missing migrations** | No | No | Yes |
| **Checks for unsafe operations** | Yes (17 rules) | Yes (~8 rules) | No |
| **Can run without database** | Yes | Yes | No |
| **Language** | Rust | Python | Python |

**When to use what:**
- Use `makemigrations --check` to ensure all model changes have migrations
- Use zdm or django-migration-linter to catch unsafe migration patterns
- zdm is useful when you want to run checks in CI without setting up Django, or when you need the additional rules (NOT NULL alterations, RenameField, irreversible migrations, RemoveIndex)

## License

MIT
