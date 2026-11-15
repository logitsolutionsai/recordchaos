"""The plugin finds the same four seeded bugs in examples/python-importer,
and none in the fixed importer. Needs the package built with maturin
(``maturin develop`` in python/)."""

from pathlib import Path

import pytest

# Run with: pytest -p pytester python/tests

ROOT = Path(__file__).resolve().parents[2]
DEMO = (ROOT / "scenarios" / "demo.toml").as_posix()
IMPORTER = (ROOT / "examples" / "python-importer").as_posix()

TEST_FILE = '''
import sys
sys.path.insert(0, {importer!r})
import recordchaos
from importer import Importer, Rejected

@recordchaos.cases({demo!r})
def test_importer(case):
    store = Importer(fixed={fixed})
    try:
        for step in case.steps:
            store.load(step)
    except Rejected:
        case.check(view={{}}, rejected=True)
        return
    case.check(view={{
        "latest_glucose": store.latest("synthetic-glucose"),
        "patient_count": store.patient_count(),
    }})
'''

BUGS = ["latest_correction_selected", "no_crash", "same_as_base", "invalid_rejected"]


def test_plugin_is_registered(pytester: pytest.Pytester) -> None:
    result = pytester.runpytest("--trace-config", "--co", "-q")
    assert "recordchaos" in result.stdout.str()


def test_finds_the_four_seeded_bugs(pytester: pytest.Pytester) -> None:
    pytester.makepyfile(test_buggy=TEST_FILE.format(importer=IMPORTER, demo=DEMO, fixed=False))
    result = pytester.runpytest("-q", "-p", "no:cacheprovider")
    out = result.stdout.str()
    for oracle in BUGS:
        assert f"check:    {oracle}" in out, f"{oracle} not found in:\n{out}"
    assert result.ret == 1


def test_fixed_importer_passes(pytester: pytest.Pytester) -> None:
    pytester.makepyfile(test_fixed=TEST_FILE.format(importer=IMPORTER, demo=DEMO, fixed=True))
    result = pytester.runpytest("-q", "-p", "no:cacheprovider")
    assert result.ret == 0, result.stdout.str()


def test_shrink_and_report(pytester: pytest.Pytester, tmp_path: Path) -> None:
    pytester.makepyfile(test_buggy=TEST_FILE.format(importer=IMPORTER, demo=DEMO, fixed=False))
    out = tmp_path / "results"
    result = pytester.runpytest("-q", "--recordchaos-shrink", f"--recordchaos-report={out}")
    assert "shrunk:" in result.stdout.str()
    assert (out / "report.html").exists()
    assert (out / "results.json").exists()
    assert (out / "junit.xml").exists()


# Regression tests from QA, 2026-11-15.


def _findings(results_json: Path) -> list:
    import json

    return json.loads(results_json.read_text(encoding="utf-8"))["findings"]


def test_one_bug_gives_one_finding(pytester: pytest.Pytester, tmp_path: Path) -> None:
    # The Python importer's urn:uuid crash raises KeyError with a different
    # generated uuid in every case. The fingerprint must not include that
    # message, or one bug splits into one finding per case (it gave 27
    # findings for 4 bugs before the fix).
    pytester.makepyfile(test_buggy=TEST_FILE.format(importer=IMPORTER, demo=DEMO, fixed=False))
    out = tmp_path / "results"
    pytester.runpytest("-q", "-p", "no:cacheprovider", f"--recordchaos-report={out}")
    findings = _findings(out / "results.json")
    oracles = sorted(f["oracle"] for f in findings)
    assert oracles == sorted(BUGS), oracles
    crash = next(f for f in findings if f["oracle"] == "no_crash")
    assert crash["sentence"].startswith("The importer raised KeyError when "), crash["sentence"]
    assert "exit" not in crash["got"], crash["got"]


def test_shrunk_finding_describes_the_shrunk_case(pytester: pytest.Pytester, tmp_path: Path) -> None:
    # With --recordchaos-shrink, the finding's sentence and diff must come from
    # the shrunk case, not from the original case with more mutations.
    pytester.makepyfile(test_buggy=TEST_FILE.format(importer=IMPORTER, demo=DEMO, fixed=False))
    out = tmp_path / "results"
    pytester.runpytest("-q", "-p", "no:cacheprovider", "--recordchaos-shrink", f"--recordchaos-report={out}")
    for f in _findings(out / "results.json"):
        assert f["mutations_after"] == 1, f
        assert " and " not in f["sentence"].split(" when ", 1)[1], f["sentence"]
        assert "changed" in f["diff_title"], f["diff_title"]


def test_refuses_a_base_bundle_with_real_looking_identifiers(pytester: pytest.Pytester) -> None:
    import json

    base = json.loads((ROOT / "fixtures" / "bundles" / "patient-a.json").read_text(encoding="utf-8"))
    for e in base["entry"]:
        if e["resource"]["resourceType"] == "Patient":
            e["resource"]["telecom"] = [{"system": "email", "value": "jane@hospital.test.io"}]
    (pytester.path / "base.json").write_text(json.dumps(base), encoding="utf-8")
    (pytester.path / "s.toml").write_text(
        'fhir_version = "4.0.1"\nbase = "base.json"\nseed = 1\ncases = 3\n\n[[operator]]\nid = "bundle.reorder_entries"\n',
        encoding="utf-8",
    )
    pytester.makepyfile(
        test_s='''
import recordchaos

@recordchaos.cases("s.toml")
def test_x(case):
    case.check(view={})
'''
    )
    refused = pytester.runpytest("-q", "-p", "no:cacheprovider")
    assert "look like real identifiers" in refused.stdout.str()
    assert refused.ret != 0
    confirmed = pytester.runpytest("-q", "-p", "no:cacheprovider", "--recordchaos-confirm-synthetic")
    assert confirmed.ret == 0, confirmed.stdout.str()


def test_refuses_a_base_bundle_with_a_broken_reference(pytester: pytest.Pytester) -> None:
    import json

    base = json.loads((ROOT / "fixtures" / "bundles" / "patient-a.json").read_text(encoding="utf-8"))
    for e in base["entry"]:
        if e["resource"]["resourceType"] == "Observation":
            e["resource"]["subject"] = {"reference": "Patient/not-in-the-bundle"}
            break
    (pytester.path / "base.json").write_text(json.dumps(base), encoding="utf-8")
    (pytester.path / "s.toml").write_text(
        'fhir_version = "4.0.1"\nbase = "base.json"\nseed = 1\ncases = 3\n\n[[operator]]\nid = "bundle.reorder_entries"\n',
        encoding="utf-8",
    )
    pytester.makepyfile(
        test_s='''
import recordchaos

@recordchaos.cases("s.toml")
def test_x(case):
    case.check(view={})
'''
    )
    result = pytester.runpytest("-q", "-p", "no:cacheprovider")
    assert "cannot be a clean baseline" in result.stdout.str(), result.stdout.str()
    assert result.ret != 0
