"""Session state shared by the pytest plugin and the case API. No native
imports here, so the plugin stays cheap when no test uses recordchaos."""

from typing import Any, Dict, List

CONFIG: Dict[str, Any] = {"shrink": False, "report": None, "confirm_synthetic": False}

# One record per checked case: {"scenario": path, "case": {...}, "result": {...}, "shrunk": [...] or None}
RECORDS: List[Dict[str, Any]] = []
