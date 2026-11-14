"""Typed failures used by the GTA capture command line."""


class GtaCaptureError(RuntimeError):
    """Base error that is safe to render to an operator."""


class ContractError(GtaCaptureError):
    """An input document violates a versioned capture contract."""


class PolicyError(GtaCaptureError):
    """The requested operation crosses a capture or delivery policy boundary."""


class BackendError(GtaCaptureError):
    """The GTA companion/backend failed or returned an invalid response."""


class StateError(GtaCaptureError):
    """A run cannot advance from its persisted state."""
