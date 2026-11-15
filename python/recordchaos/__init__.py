"""recordchaos: break synthetic HL7(R) FHIR(R) R4 patient record bundles on
purpose to test whether your importer copes with them.

The native module (``recordchaos._core``) is imported lazily, only when a
test uses it, because the pytest plugin loads in every pytest run in an
environment where this package is installed.

Synthetic test data only.
"""

from typing import Any

__all__ = ["cases", "Case", "CheckFailed", "load", "__version__"]


def __getattr__(name: str) -> Any:
    # PEP 562: keep `import recordchaos` cheap and let the pytest plugin
    # register assertion rewriting before recordchaos._api is imported.
    if name in ("cases", "Case", "CheckFailed", "load"):
        from . import _api

        return getattr(_api, name)
    if name == "__version__":
        from . import _core  # type: ignore[attr-defined]

        return _core.version()
    raise AttributeError(f"module 'recordchaos' has no attribute {name!r}")
