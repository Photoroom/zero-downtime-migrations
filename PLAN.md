# Gateway compatibility fixes for ZDM

Status: implementation in progress on `codex/gateway-compatibility`.

Commit 1: complete. The regression failed before the fix; 493 tests, Clippy,
and formatting pass. SQLAlchemy 2.0 declarative-table documentation confirms
nullable Annotated templates and explicit option precedence.
Commits 2–4: pending.

## Outcome and baseline

Gateway must be able to adopt ZDM with repository configuration and a standard
`zdm --diff <base>` command. A later, native Alembic column drop must pass after
the field was removed in earlier history. Removing the mapping and dropping the
column in the same PR must still fail.

Build on v0.9.0 (`57d4813`, release metadata following merged PR #19). The existing
`.worktrees/alembic-contract` contains that implementation. The repository root
is bare; do implementation in a worktree. This separate plan preserves the older
root PLAN.md and the completed PR #19 plan.

Evidence inspected:

- Gateway chat `01a1588c-1e26-7fc1-8ed3-0eaa1e396e84` records R005 failures for
  `manifest_generation` and `manifest_byte_length`, both `Mapped[BigInt | None]`.
- Gateway's actual `app/persistence/models.py` defines imported `BigInt` as
  `Annotated[int, mapped_column(BigInteger)]`. Its provenance model imports it.
- `src/models/mod.rs` already expands imported aliases and unions, but
  `unknown_type` rejects Annotated within a union, and metadata extraction only
  handles direct Annotated arguments. Both validation and column-name extraction
  need attention.
- The same module's `Sources::read` and `dynamic_global` conservatively mark many
  ordinary top-level expressions as unknown. The chat records this in broader
  model inventories; the exact remaining constructs need reproduction.
- `src/discovery.rs` recognizes fixed Alembic layouts. A numeric revision under
  `migrations/beta_versions/` falls through to the Aerich layout.

## Safety requirements

Preserve R005's existing merge-base AND checked-snapshot evidence contract.
Model coverage remains established by merge-base configuration. Missing mappings,
unresolved aliases, dynamic registration, ambiguous column names, and parse/read
failures must never become evidence of absence. Preserve duplicate mappings,
schemas, expansion limits, HEAD/index isolation, Django/Tortoise behavior, and
independent R008/R009 enforcement.

No application imports, database connections, per-column suppressions, checked-in
column inventories, or downstream wrapper. Static history establishes earlier
model removal; deployment and replica draining remain operational prerequisites.

## Atomic commits

### 1. `fix(models): resolve nullable column aliases`

- Add a minimal multi-file reproduction using Gateway's actual BigInt/Base and
  provenance declaration shapes before changing the resolver.
- Resolve Annotated aliases through supported optional/union forms. Share the
  annotation traversal used by validation and metadata extraction so it cannot
  accept an alias while silently losing an explicit database column name.
- Confirm supported forms against SQLAlchemy's documented behavior. Cover
  `Alias | None`, `None | Alias`, `Optional[Alias]`, and `Union[Alias, None]`;
  reject ambiguous/unsupported combinations rather than guessing a column name.
- Test imported/qualified aliases, a direct alias control, explicit DB names in
  Annotated metadata, assignment overrides, missing imports, cycles, and expansion
  limits. Assert a retained aliased `signed_manifest` blocks its own drop.
- Include unit tests and update the README's supported annotation description.

### 2. `fix(models): resolve ordinary module declarations`

- Reproduce the recorded failures using representative Gateway persistence
  modules, their local bases/mixins/aliases, and normal surrounding declarations.
  Record the fixture's source revision and included files. Check syntax first:
  the broad scan also reported invalid Python, which must not be waved through.
- Inventory each unsupported expression and add only bounded static handling
  justified by those declarations. Reuse the existing expression and symbol
  resolver; do not make every unknown call safe or add a general interpreter.
- Resolve symbols after collecting modules where necessary so results do not
  depend on file order. Distinguish proven ordinary declarations from operations
  that can register a mapper or mutate model metadata.
- Keep unsupported calls/decorators, imperative/reflected mappings, dynamic class
  creation, model attribute mutation, and unresolved local dependencies blocking.
  Add a negative test beside every relaxed classification.
- Establish complete source coverage: include all declarations of the relevant
  mappings and their dependencies. Do not fix the probe by selecting only the
  one model that makes it pass. Existing literal file paths may be used for an
  explicit complete inventory; broad `app` scanning is not a requirement to
  accept arbitrary Python, and its documented limits must be honest.
- If a necessary real construct cannot be resolved safely, document that precise
  blocker and revise this slice before implementation proceeds; do not replace
  it with a suppression. Include parser tests and supported-scope documentation.

### 3. `feat(discovery): configure Alembic locations`

- Add one explicit repository-relative directory list, proposed name
  `alembic-version-locations`, in both supported TOML formats. Gateway can list
  `migrations/versions` and `migrations/beta_versions`. Prefer this to hardcoding
  Gateway's beta name or guessing frameworks from arbitrary Python contents.
- Validate literal paths using the existing path-safety conventions; reject
  traversal, outside-repository paths, and invalid values. Treat only direct
  Python children as revisions and exclude `__init__.py`.
- Use one configured classification consistently for directory scans, explicit
  files, committed/staged diff selection, parser dispatch, changed/deleted paths,
  and migration/application classification for changeset rules. Explicit
  locations take precedence over the Aerich layout heuristic.
- Preserve existing defaults and public callers without configuration. Keep
  source-snapshot reads immutable and model-source trust rules unchanged.
- Test custom numeric and nonnumeric revisions, an unsafe Alembic operation that
  would be missed under Aerich, main and beta locations together, exclusions,
  deletions, HEAD/index modes, and unchanged Django/Aerich classification.
- Include config/discovery/CLI tests and the README configuration example.

### 4. `test(cli): cover Gateway contraction workflow`

- Build real temporary Git histories using the representative Gateway fixture,
  including the imported Base/registry, nullable Annotated alias, table
  constraints, and surrounding declarations exercised above. No Gateway runtime
  dependency or network access in the tests.
- Establish config and field removal before the contraction branch's merge-base;
  a native `op.drop_column` of `signed_manifest` must pass.
- Controls must fail: same-PR removal/drop, retained mapping, mapping reintroduced
  at HEAD/index, duplicate mapping retaining the column, unresolved dependency,
  and coverage changed only in the contraction PR. Use permissive R008 config
  where needed to prove R005 itself rejects the unsafe history.
- Exercise both main and configured beta directories, standard diff and staged
  commands, and dirty-worktree isolation. Verify diagnostics and exit status.
- Document the minimal downstream adoption: configuration established earlier,
  normal diff command, one explicit owner for the overlapping file-change
  policy, and no temporary-config generator. Gateway PR edits are separate work.

## Validation and completion

Each commit contains its own code, tests, and applicable documentation and must
pass independently. Run the focused regressions during development, then:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```

After all slices pass, run the candidate binary read-only against Gateway's
recorded immutable revisions with the proposed configuration. Reproduce both the
safe later drop and the unsafe same-PR control. Record exact SHAs, configuration,
command, and outcomes; distinguish real project validation from fixture results.
If the remote checkout is unavailable, report that unverified acceptance item.

Do not bump versions, publish a release, merge, execute DDL, or modify Gateway in
this planning task. Release metadata belongs in the later release change. This
plan does not claim that tests have run or that the failures are already fixed.
