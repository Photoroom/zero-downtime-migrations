# zero-downtime-migrations (zdm)

A PostgreSQL migration safety linter for Django, Alembic, and Aerich/Tortoise.

## Why

Deploying database migrations without downtime requires careful attention to how PostgreSQL acquires locks. Operations like adding an index, altering a column to NOT NULL, or adding a foreign key can lock tables for extended periods on large datasets, blocking reads and writes and causing outages. zdm statically analyzes Django migrations and supported Alembic and Aerich revisions to catch these unsafe patterns before they reach production, helping teams ship schema changes safely during normal deployments.

## What

A standalone Rust CLI tool that statically analyzes Django, Alembic, and Aerich migration files to catch unsafe patterns that cause table locks, outages, and data loss on large PostgreSQL databases. Distributed like ruff/uv — a single fast binary, installable via `pip`, `uvx`, or standalone download.

**Supports Django 3.2+** — zdm parses migration files directly without importing Django, so it works with any Django version and doesn't require Django to be installed.

### Alembic support

zdm also discovers Alembic revision scripts directly under `alembic/versions/*.py`. It statically supports direct `op.*` calls in `upgrade()`:

- `create_table`, `create_index`, `drop_index`
- `create_foreign_key` and `create_check_constraint` (including `postgresql_not_valid=True`), plus `create_exclude_constraint`
- `alter_column(nullable=False)` and `alter_column(new_column_name=...)`
- `drop_column` and `execute("<static SQL>")`

Use `postgresql_concurrently=True` only inside the canonical boundary:

```python
with op.get_context().autocommit_block():
    op.create_index("jobs_state_idx", "jobs", ["state"], postgresql_concurrently=True)
```

The Alembic path is intentionally static: zdm does not import or execute revision scripts, connect to a database, inspect SQLAlchemy models, resolve aliases/custom operations, or evaluate dynamic SQL. `op.execute` is inspected only when its first positional argument or `sqltext` keyword is a string literal; use explicit SQL when you want it checked.

### Aerich/Tortoise support

zdm discovers Aerich revisions under `migrations/<app>/<number>_*.py`. It inspects literal SQL returned by `upgrade()` and literal SQL passed to `execute_statement(connection, sql)` from local helpers reached by `upgrade()`, including callbacks such as `run_with_lock_timeout(db, _upgrade_attempt)`. It maps PostgreSQL `CREATE/DROP INDEX`, `CREATE TABLE`, and `ALTER TABLE` column and constraint statements to the same safety rules used for Django and Alembic.

The Aerich path is static: zdm does not import Tortoise, execute migrations, connect to a database, resolve imports, or evaluate variables and f-strings. It follows only local helper names and literal positional `execute_statement` SQL, so qualified calls, keyword arguments and dynamically assembled SQL are not checked. `CREATE TABLE IF NOT EXISTS` is not considered a fresh-table exemption. `CREATE INDEX CONCURRENTLY` and `DROP INDEX CONCURRENTLY` are accepted, while R004 does not apply because Aerich exposes no equivalent to Django's `atomic = False` or Alembic's autocommit block.

## Installation

> **Breaking change:** the `zero-downtime-migrations` command alias has been removed. Use `zdm`. (`alias zero-downtime-migrations=zdm` in your shell is a one-line workaround if you depended on the old name.)

```bash
# Install via pip
pip install zdm

# Or use uvx to run without installing
uvx --from zdm zdm .

# Or install with pipx
pipx install zdm
```

## Usage

```bash
# Lint a single migration
zdm app/migrations/0042_add_index.py
zdm alembic/versions/20260809_add_jobs.py
zdm migrations/models/1_20260823_add_jobs.py

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

# List every rule the binary recognises
zdm --list-rules

# Treat warnings as errors
zdm --warnings-as-errors .
```

`--diff` compares the merge base to `HEAD` and reads file contents from the
`HEAD` tree, giving deterministic PR/CI results even when the worktree is
dirty. `--diff-staged` compares the same merge base to the index and reads the
staged blobs.

### Exit Codes

- `0` — no issues found
- `1` — lint violations found (errors). Warnings alone do NOT cause exit code 1 unless `--warnings-as-errors` is set.
- `2` — tool error (bad arguments, config parse failure, invalid file path)

### JSON Output Schema

`zdm --output-format json` writes a single JSON object to stdout:

```json
{
  "diagnostics": [
    {
      "rule_id":   "R001",
      "rule_name": "non-concurrent-add-index",
      "severity":  "error",
      "message":   "Use AddIndexConcurrently instead of AddIndex …",
      "path":      "app/migrations/0001_bad.py",
      "line":      8,
      "column":    9,
      "help":      "Replace migrations.AddIndex with …"
    }
  ],
  "summary": { "total": 1, "errors": 1, "warnings": 0 }
}
```

`severity` is `"error"` or `"warning"`. `help` is `null` when the
rule has no help text. The schema is pinned by the integration
test suite — every field above is guaranteed on every diagnostic.

## Rules

| Rule | Name | Severity | Description |
|------|------|----------|-------------|
| R001 | non-concurrent-add-index | Error | Use `AddIndexConcurrently` instead of `AddIndex` |
| R002 | unique-constraint-without-index | Error | Unique constraints should have a concurrent index |
| R003 | runsql-create-index | Error | Use `AddIndexConcurrently` instead of raw SQL `CREATE INDEX` |
| R004 | missing-atomic-false | Error | Non-atomic migrations require `atomic = False` |
| R005 | remove-field-without-separate | Error | Use `SeparateDatabaseAndState` to remove fields safely |
| R006 | add-field-foreign-key | Error | Adding FK creates index and validates constraint, except plain FKs that disable both |
| R008 | disallowed-file-changes | Error | Don't change app code alongside migrations |
| R009 | separate-db-state-same-pr | Error | Don't deploy both steps of `SeparateDatabaseAndState` together |
| R010 | add-field-not-null | Error | Adding NOT NULL field without a Python or database default rewrites table |
| R011 | rename-field | Error | Renaming fields can break running code |
| R012 | irreversible-run-python | Warning | `RunPython` should have a reverse function |
| R013 | irreversible-run-sql | Warning | `RunSQL` should have a reverse SQL |
| R014 | model-imports | Error | Don't import models in `RunPython` |
| R015 | alter-field-not-null | Warning | `AlterField` that sets NOT NULL may scan rows, and type changes may rewrite the table |
| R016 | non-concurrent-remove-index | Error | Use `RemoveIndexConcurrently` instead of `RemoveIndex` |
| R017 | non-concurrent-add-constraint | Error | CHECK constraint validates all rows; EXCLUDE constraint builds an index non-concurrently |
| R018 | implicit-django-index | Error | `AddField` and non-empty `AlterUniqueTogether`/`AlterIndexTogether` build indexes non-concurrently; `AlterField` warns |

For Alembic revisions, zdm evaluates R001, R003-R005, R011, R015-R017 against direct `op.*` calls in `upgrade()`. For Aerich revisions, zdm evaluates R001-R002, R005-R006, R010-R011, and R015-R017 against supported literal PostgreSQL DDL reachable from `upgrade()`. In diff modes, changeset rule R008 also applies. The Django API references in this table apply only to Django; Alembic and Aerich diagnostics name their equivalent operations.

### CreateModel Exemption

Several rules (R001, R002, R006, R010, R016, R017) automatically exempt operations that target models created in the same migration. This is because operations on newly created (empty) tables don't cause the locking issues these rules detect. The exemption is order-aware—a `CreateModel` that runs *after* the flagged op cannot retroactively exempt it—and follows `RenameModel` when the fresh table is renamed before a later operation.

> **Note:** R007 (`fk-without-concurrent-index`) was merged into R006 and retired. R006 now takes the conservative stance that a prebuilt concurrent index does not make a one-step `AddField(ForeignKey)` safe on an existing table. Split the rollout instead of relying on an index exemption.

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
2. **`zero-downtime-migrations.toml`** found in the current directory or a trusted repo ancestor
3. **`pyproject.toml`** `[tool.zdm]` section in the same directory
4. **Default values**

The config search starts in the current working directory. On Unix, if zdm is running inside a trusted git repository, it walks upward within that repository and stops at the first directory that contains `zero-downtime-migrations.toml` or a `pyproject.toml` with `[tool.zdm]`. The nearest `.git` is always the boundary; an untrusted boundary never falls through to an outer repository. A pyproject for another tool is ignored, so running `zdm` from `repo/apps/myapp/migrations/` still picks up `repo/zero-downtime-migrations.toml`. Without a trusted `.git` ancestor—and on Windows, where zdm cannot yet validate repository ACL ownership—only the current directory is checked. Config inputs must be regular UTF-8 files no larger than 1 MiB.

CLI flags always override config file settings. If both `zero-downtime-migrations.toml` and `pyproject.toml` exist in the same directory, the standalone file takes precedence; multi-level merging is not performed.

## Pre-commit Integration

Install `zdm` in the environment where pre-commit runs, then call that installed
binary from your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: local
    hooks:
      - id: zdm
        name: zdm
        entry: zdm
        language: system
        types: [python]
        files: (^|.*/)(migrations|alembic/versions)/.*\.py$
        exclude: __init__\.py$
```

Or use diff mode to only check changed migrations:

```yaml
repos:
  - repo: local
    hooks:
      - id: zdm-diff
        name: zdm diff
        entry: zdm --diff-staged origin/main
        language: system
        pass_filenames: false
        always_run: true
```

The `zdm-diff` hook uses `--diff-staged` so it checks the staged index that
pre-commit is validating, rather than the previous `HEAD` commit.

The repository also publishes source-based pre-commit hooks for users who prefer
`repo: https://github.com/Photoroom/zero-downtime-migrations` with
`rev: <latest release tag>`. Those hooks install the package from source, so
Rust must be available in the pre-commit environment.

## GitHub Actions

```yaml
- uses: actions/checkout@v4
  with:
    # --diff needs the base ref and enough history to compute a merge base.
    fetch-depth: 0

- name: Install zdm
  run: pip install zdm

- name: Lint migrations
  run: zdm --diff origin/main
```

## Rust library API

The Rust crate exposes a small programmatic API, but it remains experimental
while the project is in the 0.x series. Prefer `Migration::from_path` or
`Migration::from_source`, `Config`, and the built-in rule registries. Low-level
parser, extractor, diagnostic-construction, discovery, and git helpers may
change between minor 0.x releases.

## Comparison with Other Tools

| | zdm | django-migration-linter | Django's `makemigrations --check` |
|---|---|---|---|
| **Requires Django installed** | No | Yes | Yes |
| **Requires project setup** | No | Yes (settings.py) | Yes (full environment) |
| **Checks for missing migrations** | No | No | Yes |
| **Checks for unsafe operations** | Yes (16 active rules; `zdm --list-rules`) | Yes (~8 rules) | No |
| **Configurable via `pyproject.toml`** | Yes (walks up within trusted repos) | Yes | N/A |
| **Can run without database** | Yes | Yes | No |
| **Language** | Rust | Python | Python |

**When to use what:**
- Use `makemigrations --check` to ensure all model changes have migrations
- Use zdm or django-migration-linter to catch unsafe migration patterns
- zdm is useful when you want to run checks in CI without setting up Django, or when you need the additional rules (NOT NULL alterations, RenameField, irreversible migrations, RemoveIndex)

## License

MIT
