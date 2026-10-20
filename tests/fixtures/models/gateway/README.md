# Gateway model regression fixture

Source: Photoroom/ai_gateway at
`888461880d79ee10fb5842a3cd05635fa0f153e1`.

- `generation_records.py`: unchanged persistence declarations from
  `app/ai_tools/persistence/generation_records.py`, after compatibility removal
  of `signed_manifest` in `c4f1bb55118a7c9170bcd789fbb24c3d735dc3b5`.
- `base.py`: the declaration section of `app/persistence/models.py`; unrelated
  row-validation helpers and a custom Pydantic type are omitted. No mapping used
  by these fixture models is omitted.
- `module_operations.py`: unchanged
  `app/ai_generators/persistence/module_operations.py`, covering ordinary module
  constants and an additional table sharing Base/TimestampMixin.

Tests place these sources at their original module paths inside real temporary
Git repositories. They are parsed as text, never imported. Initial mapped-field
and unsafe changes are synthesized by the tests; this is a regression fixture,
not a copy of Gateway's production Git history. Actual-source validation is
recorded separately in VALIDATION.md.
