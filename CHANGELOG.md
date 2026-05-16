# Changelog

## 0.4.0

User-visible behaviour changes; warrants a minor bump rather than a
patch.

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
- **R002, R006, R010, R016, R017** CreateModel exemptions are now
  order-aware. A `CreateModel` placed *after* the flagged op no
  longer retroactively exempts it (previously these rules consulted
  an order-blind `is_model_created` lookup). Real-world migrations
  rarely put the CreateModel below, but generated/auto-merged
  migrations sometimes do.
- **R006 (add-field-foreign-key)** exemption now requires a plain
  btree `AddIndexConcurrently` whose **first** column matches the
  FK column. A non-leading column doesn't help an FK lookup;
  partial/expression/non-btree indexes can't satisfy FK
  enforcement and the extractor doesn't see those attributes yet,
  so the help text now documents this gap.

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
- File discovery rejects symlinked migration files. A
  `0001.py -> /etc/passwd` symlink inside a migrations directory
  is no longer followed.
- Oversized migration inputs (>10 MiB) are rejected with exit
  code 2 and a `File too large:` error instead of being parsed.

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

- AST extractor surfaces `Migration.wrapped_database_ops` so rules
  can inspect operations hidden inside
  `SeparateDatabaseAndState(database_operations=[...])`. R001 is
  the first consumer.
- AST extractor captures index column lists and names on
  `IndexOperation` so R006 can do leading-column matching against
  pre-existing concurrent indexes.
- `extract_string_value` now decomposes tree-sitter `string` /
  `concatenated_string` nodes correctly, including raw-prefixed
  (`r"..."`), triple-quoted, and Python's adjacent-literal
  concatenation. F-strings with interpolations return an empty
  string rather than fabricate a plausible-but-wrong identifier.

### Migration notes

- Upgrade in a non-git workspace? You may see "config not loaded"
  if you relied on the walk-up; the walk now requires a `.git`
  ancestor. Either run zdm with the config file in the start
  directory, or `git init` the workspace.
- CI pipelines that treated R015 as a blocker should add
  `--warnings-as-errors` to preserve prior behaviour.
