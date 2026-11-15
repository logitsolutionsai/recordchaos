# Contributing

Thank you for helping. Before you open a pull request:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
```

- Synthetic data only, with made-up codes under `https://example.org/recordchaos/codes`. Never add SNOMED CT, LOINC or RxNorm codes, and never add real patient data.
- A new operator needs eligibility rules, the paths it may change, a reversibility note, an example before and after, a version number, an entry in `fixtures/operator_gold.json` and tests. Regenerate `docs/operators.md` with `recordchaos operators list --format markdown > docs/operators.md`.
- Output must stay deterministic: seeded generation, sorted output, no timestamps outside `run.toml`.
- Never edit `crates/recordchaos-core/schemas/fhir-r4/fhir.schema.json`. Local fixes go in a separate overlay file.
- The demo's terminal output and report.html are insta snapshots in `crates/recordchaos-cli/tests/snapshots/`. After an intended change, run `INSTA_UPDATE=always cargo test -p recordchaos-cli --test demo` (or `cargo insta review`) and read the diff before you commit it.
- Files a crate embeds live inside that crate (the R4 schema in `recordchaos-core`, the fonts in `recordchaos-report`, the example importer in `recordchaos-cli`), so `cargo package` and `maturin sdist` carry them.
