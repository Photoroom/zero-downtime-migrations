# zero-downtime-migrations (zdm)

A PostgreSQL migration safety linter for Django.

## Why

Deploying database migrations without downtime requires careful attention to how PostgreSQL acquires locks. Operations like adding an index, altering a column to NOT NULL, or adding a foreign key can lock tables for extended periods on large datasets, blocking reads and writes and causing outages. zdm statically analyzes Django migration files to catch these unsafe patterns before they reach production, helping teams ship schema changes safely during normal deployments.

## What

A standalone Rust CLI tool that statically analyzes Django migration files to catch unsafe patterns that cause table locks, outages, and data loss on large PostgreSQL databases. Distributed like ruff/uv — a single fast binary, installable via `pip`, `uvx`, or standalone download.

**Supports Django 3.2+** — zdm parses migration files directly without importing Django, so it works with any Django version and doesn't require Django to be installed.

## Installation

> **Breaking change:** the `zero-downtime-migrations` command alias has been removed. Use `zdm`. (`alias zero-downtime-migrations=zdm` in your shell is a one-line workaround if you depended on the old name.)

```bash
# Install via pip (PyPI package is django-zdm; binary is `zdm`)
pip install django-zdm

# Or use uvx to run without installing
uvx --from django-zdm zdm .

# Or install with pipx
pipx install django-zdm
```

## Usage

```bash
# Lint a single migration
zdm app/migrations/0042_add_index.py

# Lint all migrations in a directory
zdm app/migrations/

# Lint all migrations in the project
zdm .

# Diff mode: lint changed migrations in a PR
zdm --diff origin/main

# Staged diff mode: lint changes being committed by pre-commit
zdm --diff-staged origin/main

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
| R006 | add-field-foreign-key | Error | Adding FK creates index and validates constraint (merged R007) |
| R008 | disallowed-file-changes | Error | Don't change app code alongside migrations |
| R009 | separate-db-state-same-pr | Error | Don't deploy both steps of `SeparateDatabaseAndState` together |
| R010 | add-field-not-null | Error | Adding NOT NULL field without default rewrites table |
| R011 | rename-field | Error | Renaming fields can break running code |
| R012 | irreversible-run-python | Warning | `RunPython` should have a reverse function |
| R013 | irreversible-run-sql | Warning | `RunSQL` should have a reverse SQL |
| R014 | model-imports | Error | Don't import models in `RunPython` |
| R015 | alter-field-not-null | Warning | `AlterField` whose result is NOT NULL may scan every row |
| R016 | non-concurrent-remove-index | Error | Use `RemoveIndexConcurrently` instead of `RemoveIndex` |
| R017 | non-concurrent-add-constraint | Error | CHECK constraint validates all rows; EXCLUDE constraint builds an index non-concurrently |

### CreateModel Exemption

Several rules (R001, R002, R006, R010, R017) automatically exempt operations that target models created in the same migration. This is because operations on newly created (empty) tables don't cause the locking issues these rules detect.

> **Note:** R007 (`fk-without-concurrent-index`) was merged into R006 and retired. Its concern — that an FK on an existing table should be preceded by `AddIndexConcurrently` — is now part of R006's check, including the order-aware exemption (a concurrent index later in the same migration does not protect the FK).

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

R015 (alter-field-not-null) cannot tell, from a single `AlterField` operation, whether the column was previously nullable. It flags any `AlterField` whose resulting field is NOT NULL, which catches a genuine nullable→NOT NULL transition (the dangerous case) alongside benign re-stipulations of an already-NOT-NULL column. Because static analysis has no schema history, the rule emits `Warning` rather than `Error` — surfaced for review without breaking CI. Add `# zdm: ignore R015` on operations you have verified are safe.

### Inline Suppression

You can silence specific rules on a per-operation basis with a comment:

```python
operations = [
    # zdm: ignore R001
    migrations.AddIndex(
        model_name='order',
        index=models.Index(fields=['created_at'], name='order_idx'),
    ),
    migrations.AlterField(  # zdm: ignore R015, R010
        model_name='product',
        name='sku',
        field=models.CharField(max_length=50),
    ),
]
```

The comment can sit on the line just above the operation or on the same line as any line in the operation's range. Multiple rule IDs may be listed, separated by commas.

## Configuration

Configure via `pyproject.toml` or `zero-downtime-migrations.toml`:

```toml
[tool.zdm]
select = ["R001", "R002"]
ignore = ["R008"]
warnings-as-errors = false
allowed-file-patterns = ["*.txt", "*.md", "models.py"]
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
    rev: v0.3.2
    hooks:
      - id: zdm
```

Or use diff mode to only check changed migrations:

```yaml
repos:
  - repo: https://github.com/Photoroom/zero-downtime-migrations
    rev: v0.3.2
    hooks:
      - id: zdm-diff
```

The `zdm-diff` hook uses `--diff-staged` so it checks the staged index that
pre-commit is validating, rather than the previous `HEAD` commit.

## GitHub Actions

```yaml
- name: Install zdm
  run: pip install django-zdm

- name: Lint migrations
  run: zdm --diff origin/main
```

## Comparison with Other Tools

| | zdm | django-migration-linter | Django's `makemigrations --check` |
|---|---|---|---|
| **Requires Django installed** | No | Yes | Yes |
| **Requires project setup** | No | Yes (settings.py) | Yes (full environment) |
| **Checks for missing migrations** | No | No | Yes |
| **Checks for unsafe operations** | Yes (16 rules) | Yes (~8 rules) | No |
| **Can run without database** | Yes | Yes | No |
| **Language** | Rust | Python | Python |

**When to use what:**
- Use `makemigrations --check` to ensure all model changes have migrations
- Use zdm or django-migration-linter to catch unsafe migration patterns
- zdm is useful when you want to run checks in CI without setting up Django, or when you need the additional rules (NOT NULL alterations, RenameField, irreversible migrations, RemoveIndex)

## License

MIT
