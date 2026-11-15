"""The case API behind ``@recordchaos.cases`` and ``case.check``."""

import functools
import json
import os
import traceback
from typing import Any, Callable, Dict, List, Optional

from . import _state

_SCENARIOS: Dict[str, "Scenario"] = {}


def _core() -> Any:
    from . import _core as core  # type: ignore[attr-defined]

    return core


class CheckFailed(AssertionError):
    """A recordchaos oracle failed. The message is the finding sentence."""

    def __init__(self, message: str, result: Dict[str, Any]):
        super().__init__(message)
        self.result = result
        self.fingerprint = result.get("fingerprint")


class _Crash(Exception):
    pass


def _message(case_id: str, result: Dict[str, Any]) -> str:
    primary = result.get("primary") or {}
    lines = [result.get("sentence") or "A recordchaos check failed."]
    lines.append(f"  check:    {primary.get('oracle', '?')}")
    if primary.get("expected"):
        lines.append(f"  expected: {primary['expected']}")
    if primary.get("got"):
        lines.append(f"  got:      {primary['got']}")
    lines.append(f"  case:     {case_id}  (pytest -k {case_id})")
    return "\n".join(lines)


class Scenario:
    """A scenario file and its generated cases."""

    def __init__(self, path: str):
        self.path = path
        data = json.loads(_core().load_cases(path, None, bool(_state.CONFIG.get("confirm_synthetic"))))
        self.cases_data: List[Dict[str, Any]] = data.pop("cases")
        self.meta: Dict[str, Any] = data
        self.base: Dict[str, Any] = data["base"]
        self.baseline: Optional[Dict[str, Any]] = None
        self.by_id = {c["id"]: c for c in self.cases_data}

    def runnable(self) -> List[Dict[str, Any]]:
        return [c for c in self.cases_data if c["mutations"]]


def load(path: str) -> Scenario:
    """Load a scenario once per session."""
    key = os.path.abspath(path)
    if key not in _SCENARIOS:
        _SCENARIOS[key] = Scenario(path)
    return _SCENARIOS[key]


class Case:
    """One generated case, given to the test function."""

    def __init__(self, scenario: Scenario, data: Dict[str, Any], record: bool = True):
        self._scenario = scenario
        self._data = data
        self._record = record
        self.id: str = data["id"]
        self.bundle: Dict[str, Any] = data["bundle"]
        self.pre_steps: List[Dict[str, Any]] = data.get("pre_steps") or []
        self.operators: List[str] = [m["operator"] for m in data["mutations"]]
        self.mutations: List[Dict[str, Any]] = data["mutations"]
        self.result: Optional[Dict[str, Any]] = None

    @property
    def steps(self) -> List[Dict[str, Any]]:
        """Every bundle to load, in order: usually one, two for correction sequences."""
        return self.pre_steps + [self.bundle]

    def __repr__(self) -> str:
        return f"Case({self.id}, {'+'.join(self.operators)})"

    def check(self, view: Dict[str, Any], rejected: bool = False) -> Dict[str, Any]:
        """Run the oracles on the view your importer reports. Raises
        CheckFailed with the finding sentence when a check fails."""
        return self._check(view, "rejected" if rejected else "accepted", "")

    def _check(self, view: Dict[str, Any], outcome: str, first_line: str) -> Dict[str, Any]:
        baseline = self._scenario.baseline
        if baseline is None:
            raise RuntimeError("recordchaos: the baseline view was not captured")
        out = json.loads(
            _core().check(
                json.dumps(self._data),
                json.dumps(view, default=str),
                json.dumps(baseline, default=str),
                outcome,
                first_line,
            )
        )
        self.result = out
        if self._record:
            _state.RECORDS.append({"scenario": self._scenario.path, "case": self._data, "result": out, "shrunk": None})
        if out.get("primary"):
            raise CheckFailed(_message(self.id, out), out)
        return out


class _BaselineCase(Case):
    """Given to the test function once, with the base bundle, to capture the
    baseline view that most oracles compare against."""

    def __init__(self, scenario: Scenario):
        data = {"id": "baseline", "bundle": scenario.base, "pre_steps": [], "mutations": []}
        super().__init__(scenario, data, record=False)

    def check(self, view: Dict[str, Any], rejected: bool = False) -> Dict[str, Any]:
        if rejected:
            raise RuntimeError("recordchaos: the importer rejected the base bundle; it must accept a clean baseline")
        self._scenario.baseline = json.loads(json.dumps(view, default=str))
        return {"baseline": self._scenario.baseline}


def _crash_line(e: BaseException) -> str:
    """A stable error line for an exception: its type and where it was raised.

    The message is left out on purpose. It often holds values that change from
    case to case (a generated urn:uuid, an id), and this line is part of the
    failure's fingerprint, so one bug must give one line. The full message is
    still in the pytest traceback."""
    frames = traceback.extract_tb(e.__traceback__)
    where = ""
    if frames:
        last = frames[-1]
        where = f" at {os.path.basename(last.filename)}:{last.lineno}"
    return f"{type(e).__name__}{where}"[:200]


def _run_one(fn: Callable[..., Any], case: Case, args: Any, kwargs: Any) -> None:
    """Call the test function; an exception from the importer is a crash."""
    try:
        fn(case, *args, **kwargs)
    except CheckFailed:
        raise
    except AssertionError:
        raise
    except Exception as e:  # the importer crashed
        if case.result is not None:
            raise
        try:
            case._check({}, "crashed", _crash_line(e))
        except CheckFailed as failed:
            raise failed from e


def _ddmin(items: List[Any], fails: Callable[[List[Any]], bool]) -> List[Any]:
    current = list(items)
    n = 2
    while len(current) >= 2:
        chunk = -(-len(current) // n)
        subsets = [current[i : i + chunk] for i in range(0, len(current), chunk)]
        reduced = False
        for s in subsets:
            if fails(s):
                current, n, reduced = s, 2, True
                break
        if not reduced and len(subsets) > 2:
            for i in range(len(subsets)):
                comp = [x for j, s in enumerate(subsets) if j != i for x in s]
                if fails(comp):
                    current, n, reduced = comp, max(n - 1, 2), True
                    break
        if not reduced:
            if n >= len(current):
                break
            n = min(n * 2, len(current))
    return current


def _shrink(fn: Callable[..., Any], scen: Scenario, case: Case, failed: CheckFailed, args: Any, kwargs: Any) -> CheckFailed:
    target = failed.fingerprint
    budget = [64]
    # The last failing trial with the target fingerprint, and its case data.
    last: List[Optional[CheckFailed]] = [None]
    last_data: List[Optional[Dict[str, Any]]] = [None]

    def fails(subset: List[Dict[str, Any]]) -> bool:
        if not subset or budget[0] <= 0:
            return False
        budget[0] -= 1
        try:
            derived = json.loads(_core().derive_case(json.dumps(scen.base), json.dumps(subset), scen.meta["as_of"]))
        except ValueError:
            return False
        data = dict(case._data, bundle=derived["bundle"], pre_steps=derived["pre_steps"], mutations=derived["mutations"])
        trial = Case(scen, data, record=False)
        try:
            _run_one(fn, trial, args, kwargs)
        except CheckFailed as f:
            if f.fingerprint == target:
                last[0] = f
                last_data[0] = data
                return True
        except Exception:
            return False
        return False

    kept = _ddmin(case.mutations, fails)
    shrunk_case = last_data[0]
    if len(kept) < len(case.mutations) and last[0] is not None and shrunk_case is not None:
        if _state.RECORDS and _state.RECORDS[-1]["case"] is case._data:
            # The report shows the shrunk case: its sentence, bundle and diff.
            _state.RECORDS[-1]["shrunk"] = shrunk_case["mutations"]
            _state.RECORDS[-1]["shrunk_bundle"] = shrunk_case["bundle"]
            _state.RECORDS[-1]["shrunk_result"] = last[0].result
        msg = str(last[0]) + f"\n  shrunk:   from {len(case.mutations)} mutations to {len(kept)}: {', '.join(m['operator'] for m in kept)}"
        return CheckFailed(msg, last[0].result)
    return failed


def cases(path: str) -> Callable[[Callable[..., Any]], Any]:
    """Parametrize a test with one pytest item per case, named by case id and
    operator. The test function takes ``case`` as its first argument."""

    def decorate(fn: Callable[..., Any]) -> Any:
        import pytest

        scen = load(path)
        runnable = scen.runnable()
        ids = [c["id"] for c in runnable]
        labels = [f"{c['id']}-{'+'.join(m['operator'] for m in c['mutations'])}" for c in runnable]

        @functools.wraps(fn)
        def wrapper(case: str, *args: Any, **kwargs: Any) -> None:
            if scen.baseline is None:
                fn(_BaselineCase(scen), *args, **kwargs)
            c = Case(scen, scen.by_id[case])
            try:
                _run_one(fn, c, args, kwargs)
            except CheckFailed as failed:
                if _state.CONFIG.get("shrink") and len(c.mutations) > 1:
                    raise _shrink(fn, scen, c, failed, args, kwargs) from None
                raise

        marked = pytest.mark.recordchaos(wrapper)
        return pytest.mark.parametrize("case", ids, ids=labels)(marked)

    return decorate


def write_report(out_dir: str) -> str:
    """Write report.html, results.json and junit.xml for the session."""
    os.makedirs(out_dir, exist_ok=True)
    by_scenario: Dict[str, List[Dict[str, Any]]] = {}
    for r in _state.RECORDS:
        by_scenario.setdefault(r["scenario"], []).append(r)
    verdicts = []
    for i, (path, records) in enumerate(sorted(by_scenario.items())):
        scen = load(path)
        target = out_dir if len(by_scenario) == 1 else os.path.join(out_dir, f"scenario-{i + 1}")
        payload = {"scenario": scen.meta, "records": records}
        verdicts.append(_core().render_report(json.dumps(payload, default=str), target))
    return "; ".join(verdicts)
