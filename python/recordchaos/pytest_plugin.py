"""pytest plugin for recordchaos, registered through the pytest11 entry point.

Check that it is registered with ``pytest --trace-config``.
"""

import pytest

# Must run before recordchaos._api is imported, so its assertions read well.
pytest.register_assert_rewrite("recordchaos._api")


def pytest_addoption(parser: "pytest.Parser") -> None:
    group = parser.getgroup("recordchaos", "recordchaos: synthetic record chaos tests")
    group.addoption(
        "--recordchaos-shrink",
        action="store_true",
        default=False,
        help="Re-run a failing recordchaos case on smaller bundles until the fewest mutations that still fail are left.",
    )
    group.addoption(
        "--recordchaos-report",
        action="store",
        default=None,
        metavar="DIR",
        help="Write report.html, results.json and junit.xml for recordchaos cases to DIR.",
    )
    group.addoption(
        "--recordchaos-confirm-synthetic",
        action="store_true",
        default=False,
        help="Continue when a base bundle has values that look like real identifiers. Use synthetic data only.",
    )


def pytest_configure(config: "pytest.Config") -> None:
    from . import _state

    _state.CONFIG["shrink"] = bool(config.getoption("--recordchaos-shrink"))
    _state.CONFIG["report"] = config.getoption("--recordchaos-report")
    _state.CONFIG["confirm_synthetic"] = bool(config.getoption("--recordchaos-confirm-synthetic"))
    config.addinivalue_line("markers", "recordchaos: a test parametrized with recordchaos cases")


def pytest_sessionfinish(session: "pytest.Session", exitstatus: int) -> None:
    from . import _state

    out = _state.CONFIG.get("report")
    if not out or not _state.RECORDS:
        return
    from . import _api

    verdict = _api.write_report(out)
    reporter = session.config.pluginmanager.get_plugin("terminalreporter")
    if reporter is not None:
        reporter.write_line(f"recordchaos: {verdict}. Report in {out}/report.html")
