"""sandstorm — Python client for a Sandstorm master.

Sandstorm runs sandboxes on gVisor or Firecracker across a fleet of nodes,
and suspends the idle ones so one machine holds far more than it could run
at once. This SDK talks to the master over its HTTP API and provides two
surfaces:

- ``Sandbox`` (E2B-compatible) — create one sandbox from a template or a
  snapshot tag, run code in it, and kill it.
- ``Controller`` — the API itself: snapshots, sandboxes, fan-out and
  branching across the fleet.

``Sandbox`` is built on ``Controller``. Start with ``Sandbox``, and use
``Controller`` for what goes beyond one sandbox.
"""

from .controller import (
    SANDBOX_TOKEN_HEADER,
    Controller,
    ControllerError,
)
from .sandbox import CommandResult, Sandbox

__version__ = "0.6.0"
__all__ = [
    "Sandbox",
    "CommandResult",
    "Controller",
    "ControllerError",
    "SANDBOX_TOKEN_HEADER",
]
