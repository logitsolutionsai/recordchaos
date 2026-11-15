"""A small FHIR R4 importer with the same four seeded bugs as the Rust
example (crates/recordchaos-cli/examples/buggy_importer.rs), used by the pytest plugin tests.

1. Keeps the first Observation seen for a code and effective time, so a
   correction or a final version is ignored (operators 1 and 2).
2. Crashes on urn:uuid references (operator 15).
3. Treats entry order as time order (operator 19).
4. Accepts dangling references silently (operator 17).

``Importer(fixed=True)`` has all four fixed. Synthetic test data only.
"""

from datetime import datetime, timezone
from typing import Any, Dict, List, Optional


class Rejected(Exception):
    """The importer refused the bundle, with a reason."""


def _epoch(text: Optional[str]) -> float:
    if not text:
        return float("-inf")
    t = text.replace("Z", "+00:00")
    if len(t) == 10:
        t += "T00:00:00+00:00"
    try:
        d = datetime.fromisoformat(t)
    except ValueError:
        return float("-inf")
    if d.tzinfo is None:
        d = d.replace(tzinfo=timezone.utc)
    return d.timestamp()


def _effective(res: Dict[str, Any]) -> Optional[str]:
    return res.get("effectiveDateTime") or res.get("effectiveInstant") or (res.get("effectivePeriod") or {}).get("start")


def _code(res: Dict[str, Any]) -> Optional[str]:
    for c in (res.get("code") or {}).get("coding") or []:
        if c.get("code"):
            return c["code"]
    return None


def _references(value: Any, out: List[str]) -> None:
    if isinstance(value, dict):
        ref = value.get("reference")
        if isinstance(ref, str):
            out.append(ref)
        for k, v in value.items():
            if k != "reference":
                _references(v, out)
    elif isinstance(value, list):
        for v in value:
            _references(v, out)


class Importer:
    def __init__(self, fixed: bool = False):
        self.fixed = fixed
        self.patients: List[str] = []
        self.observations: List[Dict[str, Any]] = []

    def load(self, bundle: Dict[str, Any]) -> None:
        entries = bundle.get("entry") or []
        full_urls: Dict[str, Dict[str, Any]] = {}
        by_key: Dict[str, Dict[str, Any]] = {}
        for e in entries:
            res = e.get("resource") or {}
            if e.get("fullUrl"):
                if e["fullUrl"] in full_urls:
                    raise Rejected("two entries share one fullUrl")
                full_urls[e["fullUrl"]] = res
            if res.get("resourceType") and res.get("id"):
                by_key[f"{res['resourceType']}/{res['id']}"] = res
        for e in entries:
            res = e.get("resource") or {}
            if res.get("resourceType") == "Observation":
                if "code" not in res:
                    raise Rejected("Observation without code")
                if "status" not in res:
                    raise Rejected("Observation without status")
            if res.get("resourceType") == "Encounter":
                p = res.get("period") or {}
                if p.get("end") and _epoch(p["end"]) < _epoch(p.get("start")):
                    raise Rejected("Encounter ends before it starts")

        def resolve(ref: str) -> Optional[Dict[str, Any]]:
            if ref.startswith("urn:uuid:"):
                if not self.fixed:
                    # Bug 2: only Type/id and URLs are in the lookup table.
                    return by_key[ref]
                return full_urls.get(ref)
            if ref in full_urls:
                return full_urls[ref]
            parts = ref.split("/")
            if len(parts) >= 2:
                return by_key.get("/".join(parts[-2:]))
            return None

        def patient_of(res: Dict[str, Any]) -> Optional[str]:
            if res.get("resourceType") == "Patient":
                return res.get("id")
            subj = (res.get("subject") or res.get("patient") or {}).get("reference")
            if not subj:
                return None
            target = resolve(subj)
            if target and target.get("resourceType") == "Patient":
                return target.get("id")
            return None

        for e in entries:
            res = e.get("resource") or {}
            refs: List[str] = []
            _references(res, refs)
            for r in refs:
                if r.startswith("#"):
                    continue
                if resolve(r) is None and self.fixed:
                    # Bug 4 is this check missing in the buggy importer.
                    raise Rejected(f"{r} does not resolve inside the bundle")
            if res.get("resourceType") in (None, "Patient"):
                continue
            mine = patient_of(res)
            linked = [(res.get("encounter") or {}).get("reference")]
            if res.get("resourceType") == "DiagnosticReport":
                linked += [item.get("reference") for item in res.get("result") or []]
            for r in linked:
                other = resolve(r) if r else None
                if mine and other is not None and patient_of(other) not in (None, mine):
                    raise Rejected("a resource links records of two patients")

        for e in entries:
            res = e.get("resource") or {}
            if res.get("resourceType") == "Patient" and res.get("id") not in self.patients:
                self.patients.append(res["id"])
            if res.get("resourceType") != "Observation":
                continue
            code, eff = _code(res), _effective(res)
            if not code or not eff:
                continue
            value = (res.get("valueQuantity") or {}).get("value", res.get("valueString"))
            obs = {
                "id": res.get("id"),
                "code": code,
                "effective": eff,
                "effective_epoch": _epoch(eff),
                "issued_epoch": _epoch(res.get("issued")),
                "status": res.get("status", ""),
                "value": value,
            }
            if self.fixed:
                same_id = [i for i, o in enumerate(self.observations) if o["id"] == obs["id"]]
                if same_id:
                    self.observations[same_id[0]] = obs
                    continue
                same_time = [
                    i
                    for i, o in enumerate(self.observations)
                    if o["code"] == code and o["effective_epoch"] == obs["effective_epoch"]
                ]
                if same_time:
                    old = self.observations[same_time[0]]
                    if obs["issued_epoch"] >= old["issued_epoch"]:
                        self.observations[same_time[0]] = obs
                    continue
                self.observations.append(obs)
            else:
                # Bug 1: the first Observation for a code and effective time wins.
                seen = any(o["id"] == obs["id"] or (o["code"] == code and o["effective"] == eff) for o in self.observations)
                if not seen:
                    self.observations.append(obs)

    def latest(self, code: str) -> Optional[Dict[str, Any]]:
        found = None
        for o in self.observations:
            if o["code"] != code:
                continue
            if not self.fixed:
                found = o  # Bug 3: the last one in entry order is "latest".
            elif found is None or (o["effective_epoch"], o["issued_epoch"]) > (found["effective_epoch"], found["issued_epoch"]):
                found = o
        return None if found is None else {"id": found["id"], "value": found["value"]}

    def patient_count(self) -> int:
        return len(self.patients)
