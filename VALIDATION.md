# Gateway compatibility validation

Validated locally on 2026-10-20. This is static lint validation; no migration
was executed, and no database or application runtime was accessed.

## Automated regression suite

`cargo test --locked --all-features` passes 506 tests. Formatting and
`cargo clippy --locked --all-targets --all-features -- -D warnings` pass.
Each implementation commit was tested independently, including after amending
the alias commit with negative tests for unknown explicit column values.

The checked-in Gateway fixture tests both normal and beta revision directories,
HEAD/index evidence, dirty-worktree isolation, earlier removal, same-PR removal,
retained/reintroduced/duplicate mappings, missing dependencies, and changed
coverage. Parser regressions were observed failing before their fixes.

## Actual Gateway sources

Temporary shared local clones retain the actual Gateway Git history. They use
the full, unchanged `app/persistence/models.py` and the original generation
records and module-operation sources, rather than the shortened fixture base.
Only proposed ZDM configuration and synthetic static probes are committed in
these disposable clones. No changes were made to the original Gateway checkout.

Candidate source: `8c87d96` (the final test/docs commit does not change the binary).
Candidate debug binary SHA-256: `67f2e92339a25c4c7db064a9000283910ef9f1c42cdd499a986183cf03044ffb`.

| Scenario | Original Gateway revision | Exit: HEAD / staged |
| --- | --- | --- |
| earlier-removal | `888461880d79ee10fb5842a3cd05635fa0f153e1` | 0 / 0 |
| same-pr-removal | `bc4a4e49564406be75bec2f9f4149dbf01f68c69` | 1 / 1 |

Safe source history contains compatibility removal
`c4f1bb55118a7c9170bcd789fbb24c3d735dc3b5`. The unsafe control starts from
its actual parent and copies the subsequent generation-record removal into the
same changeset as the drop. Both main and beta directories receive a probe:

```python
from alembic import op
revision = 'zdm_validation_probe'
down_revision = None
def upgrade():
    op.drop_column('ai_tool_generation_provenance', 'signed_manifest')
def downgrade():
    pass
```

The probes are parsed only. Adopted configuration, committed before the probe:

```toml
alembic-version-locations = ["migrations/versions", "migrations/beta_versions"]
allowed-file-patterns = ["app/**"]
[[model-sources]]
orm = "sqlalchemy"
paths = ["app/persistence/models.py", "app/ai_tools/persistence/generation_records.py", "app/ai_generators/persistence/module_operations.py"]
```

`allowed-file-patterns = ["app/**"]` deliberately isolates R005 in this
validation so the unsafe control cannot fail solely through R008. It is not
a recommendation to allow every application change in production.

Each clone runs the standard commands with all rules enabled:

```sh
zdm --diff zdm-validation-base --output-format json
zdm --diff-staged zdm-validation-base --output-format json
```

Synthetic local commit receipts:

- earlier-removal: adoption/merge-base `13f12e10e5fcc181f14c3bf3da6c6d9a6940b8ce`;
  probe HEAD `f72f6bb8a16c6f5a524740397380c701a4dbe86a`.
- same-pr-removal: adoption/merge-base `41d0fb6f36c34ead21f033e1a677c2a834f46127`;
  probe HEAD `95f92e470446d02b4aa2b1dad4327c1cd4a6f302`.

The existing v0.9.0 binary fails the safe case with R005 for ordinary module
expressions and misses the beta revision. The candidate returns no diagnostics
for that case. For the unsafe control, the candidate reports R005 for both
directories because `signed_manifest` is still mapped at the merge-base.

## Coverage and downstream adoption

Repository declaration searches locate the target physical table in
`app/ai_tools/persistence/generation_records.py` in both source snapshots.
The validation explicitly declares that file and its imported Base/alias
definitions; the module-operation file additionally exercises ordinary constants.
This is a declared table-mapping boundary, not proof of complete service coverage.

A separate full runtime declaration probe selected 17 mapping/base files at
`888461880d79ee10fb5842a3cd05635fa0f153e1`. That inventory remains blocked by
unsupported dataclass decorators, enum bases, Pydantic TypeAdapter construction,
and missing local mixin/exception dependencies. Broad `paths = ["app"]` is not
supported for Gateway. These constructs have not been silently ignored. Before
expanding coverage, include the dependencies and add bounded parser support.

Gateway adoption is a separate change: pin the eventual upstream release,
establish the explicit model inventory and Alembic directories before the
contraction PR, and run `zdm --diff <base>` in the actual repository at the
intended immutable HEAD. Keep the required CI job and its revision verification.
Use ZDM R008 as the separation-policy owner for both directories, configure its
approved file patterns, and remove overlapping policy enforcement during that
adoption. The existing Gateway policy only identifies the main versions
directory, so disabling R008 alone would leave beta without that policy.
No temporary migration copy or generated configuration is needed.

Deployment/replica checks and review of consumers outside declared ORM coverage
remain operational work. No version bump, release, merge, or Gateway modification
is included here.
