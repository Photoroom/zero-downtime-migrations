# Changelog

## 0.6.0 - 2026-08-24

### Added

- zdm now analyses PostgreSQL migrations from Django, Alembic/SQLAlchemy, and
  Aerich/Tortoise.
- Aerich support analyses supported static DDL, including locally defined
  helper callbacks and literal `execute_statement(...)` calls. It detects
  unsafe indexes, destructive changes, type changes, foreign keys, and
  constraints, while recognising concurrent indexes and `NOT VALID`
  constraints.

### Packaging

- The PyPI distribution is now `zdm`.

## 0.5.0 - 2026-08-11

### Added

- Alembic revision scripts under `alembic/versions/` are now checked for the
  supported zero-downtime migration rules alongside Django migrations.
- Alembic support covers direct, unconditional `op.*` calls in `upgrade()`;
  `postgresql_not_valid=True` constraints and `autocommit_block()` are handled
  as safe PostgreSQL migration patterns.

### Documentation

- Document Alembic usage, supported scope, and pre-commit discovery.

## 0.4.0 - 2026-07-10

Changes since 0.3.2:

### Rules

- **R015 (alter-field-not-null) is now a Warning, not an Error.** A
  no-op `AlterField` (changing `max_length`/`help_text` on an
  already-NOT-NULL column) was previously flagged as an Error;
  Django often emits these and the rule has no way to tell a
  rewrite from a metadata change. Severity flip keeps the
  diagnostic but stops blocking CI by default. Run with
  `--warnings-as-errors` to restore the prior gate.
- **R017 (non-concurrent-add-constraint)** now also flags
  `ExclusionConstraint`, not just `CheckConstraint` and
  `UniqueConstraint`. EXCLUDE constraints build their index
  non-concurrently under ACCESS EXCLUSIVE; the help text documents
  the (limited) mitigations because Postgres has no `NOT VALID`
  form for EXCLUDE.
- **R001 (non-concurrent-add-index)** now also flags `AddIndex`
  wrapped inside `SeparateDatabaseAndState(database_operations=[...])`.
  Wrapping doesn't defang the lock — Django runs the wrapped op
  against the live schema.
- **R003, R012, R013, and R015** now also inspect literal
  `SeparateDatabaseAndState(database_operations=[...])` operations.
  Diagnostics keep the inner operation span so inline ignores attach
  to the operation being reported.
- **R001, R002, R006, R010, R016, R017** CreateModel exemptions are now
  order-aware. A `CreateModel` placed *after* the flagged op no
  longer retroactively exempts it (previously these rules consulted
  an order-blind `is_model_created` lookup). Real-world migrations
  rarely put the CreateModel below, but generated/auto-merged
  migrations sometimes do.
- **R006 (add-field-foreign-key)** now deliberately ignores prebuilt
  index exemptions. Even a matching `AddIndexConcurrently` does not
  silence a one-step `AddField(ForeignKey)` on an existing table;
  split the column, index, backfill, and constraint work explicitly.
- **R003** resolves the final module-level SQL assignment that appears before
  the migration class. Later dynamic assignments no longer fall back to stale
  earlier literals, preventing reassignment from hiding a blocking index.
- Fresh-model exemptions follow `RenameModel`, so an empty table remains
  exempt under its new model name.
- **R008** now treats deleted application files as changes alongside a
  migration instead of silently dropping them from the changeset.
- Programmatic R008 configurations with an invalid allowed-file glob now fail
  closed instead of panicking.

### CLI / output

- New `--list-rules` flag prints every rule the binary knows about
  (ID, severity, name). Short-circuits before config load, so it
  works outside a configured project.
- `Error::UnknownRule` (raised by `zdm rule <unknown>`) was
  multi-line; it's now single-line so the security-sanitization
  pass doesn't mangle the layout.
- Diagnostic output now escapes ASCII control characters (ANSI
  sequences, BEL, backspace, CR) before rendering. Hostile model
  names and filenames can no longer repaint the terminal.
  Newlines and tabs are escaped in paths, error chain strings,
  and rule messages (single-line by convention); help text
  preserves newlines for multi-line layout.
- C1 Unicode control characters are escaped as well.
- File discovery rejects symlinked migration files. A
  `0001.py -> /etc/passwd` symlink inside a migrations directory
  is no longer followed.
- Oversized migration inputs (>10 MiB) are rejected with exit
  code 2 and a `File too large:` error instead of being parsed.
- `--diff` reads migration content from the tree at `HEAD`; unstaged working
  tree edits cannot hide a violation in the commit being compared. Staged mode
  continues to read the index.
- Pathologically deep Python ASTs are rejected before extraction, bounding
  stack use for untrusted migration files. Module-level SQL bindings are
  indexed once, avoiding quadratic rescans in identifier-heavy migrations.
- Invalid git references include the fetch/shallow-checkout recovery hint in
  normal CLI output.

### Configuration

- `Config::load_from_directory` now walks upward from the start
  directory to find a `pyproject.toml` / `zero-downtime-migrations.toml`
  in any ancestor — but only inside a confirmed git repository.
  The walk requires a `.git` ancestor (directory or worktree
  file) and stops at the repo root. Outside a repo, only the
  start directory is consulted. This closes a silent escalation
  path where a world-writable `/tmp/zero-downtime-migrations.toml`
  could be adopted as "the project config" when running zdm in a
  non-repo directory.
- `Config.exclude` and `Config.allowed_file_patterns` are validated
  at load time; an invalid glob now surfaces as
  `Error::InvalidGlobPattern` instead of being silently dropped.
- Config discovery stops at the nearest `.git` even when that boundary is
  untrusted, and config files must be regular UTF-8 files no larger than
  1 MiB. Upward discovery fails closed on Windows until ACL ownership can be
  validated; current-directory config remains supported.

### Distribution

- Release tags must point to a commit on `main` and pass the locked RustSec
  audit before any artifacts are built.
- Wheels and source distributions are installed and smoke-tested before
  publication. Standalone binaries are extracted from the same platform wheels,
  so Linux downloads inherit the declared manylinux 2.28 compatibility floor.
- GitHub releases include `SHA256SUMS`, and `install.sh` verifies the selected
  standalone binary before installing or executing it.

### JSON output schema

```json
{
  "diagnostics": [
    {
      "rule_id":   "R001",
      "rule_name": "non-concurrent-add-index",
      "severity":  "error",          // or "warning"
      "message":   "...",
      "path":      "app/migrations/0001_bad.py",
      "line":      8,
      "column":    9,
      "help":      "..."             // null when the rule has no help
    }
  ],
  "summary": {
    "total":    1,
    "errors":   1,
    "warnings": 0
  }
}
```

The schema is pinned by `json_output_contains_required_fields`
in the integration test suite — fields above are guaranteed to
be present on every diagnostic.

### Internal

- AST extractor surfaces database-effective operations hidden inside
  `SeparateDatabaseAndState(database_operations=[...])` so rules can
  inspect them in execution order while preserving each operation's
  source span.
- `extract_string_value` now decomposes tree-sitter `string` /
  `concatenated_string` nodes correctly, including raw-prefixed
  (`r"..."`), triple-quoted, and Python's adjacent-literal
  concatenation. F-strings with interpolations return an empty
  string rather than fabricate a plausible-but-wrong identifier.
- Normal directory scans release each parsed migration immediately instead of
  retaining the entire repository in memory. The unused `miette/fancy`
  rendering stack was removed; user-facing recovery guidance now lives in the
  errors the CLI actually prints.

### Migration notes

- **Rust library API:** v0.4 deliberately replaces the broad experimental
  v0.3 surface with a smaller one. Programmatic consumers should parse through
  `Migration::from_path` / `Migration::from_source`, configure through
  `Config`, and run built-in rules through `RuleRegistry` or
  `ChangesetRuleRegistry`. Low-level parser, extractor, diagnostic-builder,
  discovery, and git convenience APIs are no longer compatibility promises
  during the 0.x series.

- Upgrade in a non-git workspace? You may see "config not loaded"
  if you relied on the walk-up; the walk now requires a `.git`
  ancestor. Either run zdm with the config file in the start
  directory, or `git init` the workspace.
- CI pipelines that treated R015 as a blocker should add
  `--warnings-as-errors` to preserve prior behaviour.
