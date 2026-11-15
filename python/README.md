# recordchaos for Python

The Python package and pytest plugin for [recordchaos](https://github.com/logitsolutionsai/recordchaos), built from the same Rust core as the command-line tool.

Status: planned. Nothing is published yet. Until then, build it from the root of a clone of the repository (needs Rust):

```bash
pip install ./python
```

```python
import recordchaos

@recordchaos.cases("scenarios/late-correction.toml")
def test_importer(case):
    store = MyImporter()
    for step in case.steps:              # usually one bundle; two for correction sequences
        store.load(step)                 # dict
    case.check(view={
        "latest_glucose": store.latest("synthetic-glucose"),
        "patient_count": store.patient_count(),
    })
```

- Each case becomes one pytest item, named by case id and operator.
- The scenario path is relative to the folder you run pytest from.
- `case.check(view=...)` runs the same oracles as the command-line tool and raises a readable assertion with the finding sentence.
- If your importer rejects a bundle, call `case.check(view={}, rejected=True)`. Any other exception from your importer counts as a crash. Its fingerprint is the exception type and the file and line that raised it, so one bug gives one finding even when the message changes from case to case.
- `pytest --recordchaos-shrink` re-runs a failing test with fewer mutations until the fewest that still fail are left.
- `pytest --recordchaos-report results/` writes the same report.html, results.json and junit.xml.
- Like `recordchaos run`, the plugin refuses a base bundle that fails the schema, has a broken reference, or has values that look like real identifiers. Pass `--recordchaos-confirm-synthetic` only for a bundle you know is synthetic; the check is a heuristic and does not prove that a bundle is de-identified.
- Check that the plugin is registered with `pytest --trace-config`.

Synthetic test data only. HL7, FHIR and the FHIR [FLAME DESIGN] are the registered trademarks of Health Level Seven International and their use does not constitute endorsement by HL7.
