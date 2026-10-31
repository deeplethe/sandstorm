"""E2B-compatible Sandbox wrapper around a sandstorm master's HTTP API."""

from __future__ import annotations

import os
import time
from dataclasses import dataclass
from typing import Any, Optional, Sequence, Union

from .controller import Controller, ControllerError

#: The master stops retrying a sandbox that has failed to start this many
#: times (``MAX_FAILURES`` in ``master/api.rs``), so waiting longer is futile.
_MAX_START_FAILURES = 5


@dataclass
class CommandResult:
    """Result of `sandbox.commands.run(...)`. Mirrors E2B's API."""

    stdout: str
    stderr: str
    exit_code: int


class _CommandsProxy:
    """Implements the `sandbox.commands` namespace from E2B's API."""

    def __init__(self, sandbox: "Sandbox") -> None:
        self._sandbox = sandbox

    def run(
        self,
        cmd: Union[str, Sequence[str]],
        timeout: Optional[int] = None,
    ) -> CommandResult:
        """Run a command inside the sandbox and return its output.

        `cmd` can be a string (executed via `sh -c`) or a list/tuple of
        argv tokens (executed directly). `timeout` is in seconds and
        defaults to the sandbox's own.
        """
        if isinstance(cmd, str):
            args = ["/bin/sh", "-c", cmd]
        else:
            args = list(cmd)
        sb = self._sandbox
        resp = sb.controller.exec_command(
            sb.id,
            args,
            timeout_secs=sb.timeout if timeout is None else timeout,
            sandbox_token=sb.token,
        )
        return CommandResult(
            stdout=resp.get("stdout", ""),
            stderr=resp.get("stderr", ""),
            exit_code=int(resp.get("exit_code", -1)),
        )


class Sandbox:
    """One sandbox on a sandstorm master, E2B-compatible surface.

    Creates the sandbox from a registered template, or forks it from a
    snapshot tag, and returns once it is running. The master is the one
    `Controller` reads from ``$SANDSTORM_URL``, ``$SANDSTORM_TOKEN`` and
    ``$SANDSTORM_CA``; pass ``controller=`` to reach another.

    Parameters
    ----------
    template:
        Template to start from. Defaults to ``$SANDSTORM_TEMPLATE``.
    tag:
        Snapshot tag to fork from instead. Defaults to ``$SANDSTORM_TAG``.
        Give a template or a tag, not both.
    timeout:
        Default time limit for a command or eval, in seconds.
    start_timeout:
        How long to wait for the sandbox to start, in seconds.
    memory_limit_mib:
        Memory ceiling for this sandbox, overriding the template's.
    controller:
        Client for the master. Defaults to ``Controller()``.

    Example
    -------
    >>> with Sandbox("python") as sb:
    ...     print(sb.commands.run("echo hi").stdout)
    hi
    """

    def __init__(
        self,
        template: Optional[str] = None,
        *,
        tag: Optional[str] = None,
        timeout: int = 30,
        start_timeout: float = 60.0,
        memory_limit_mib: Optional[int] = None,
        controller: Optional[Controller] = None,
    ) -> None:
        if template is None and tag is None:
            template = os.environ.get("SANDSTORM_TEMPLATE")
            tag = os.environ.get("SANDSTORM_TAG")
        if (template is None) == (tag is None):
            raise ValueError(
                "give a template or a tag, not both or neither; "
                "SANDSTORM_TEMPLATE and SANDSTORM_TAG set the same defaults"
            )
        controller = controller or Controller()
        info = controller.create_sandbox(
            template=template,
            snapshot_tag=tag,
            memory_limit_mib=memory_limit_mib,
        )
        self._bind(controller, timeout, info)
        try:
            self._wait_until_running(start_timeout)
        except BaseException:
            self._discard()
            raise

    # ----- public API -----------------------------------------------------

    @property
    def sandbox_id(self) -> str:
        """E2B's name for `id`."""
        return self.id

    def eval(self, code: str) -> str:
        """Run a code snippet in the sandbox and return what it printed.

        Not part of E2B's API. The master appends `code` to the template's
        ``eval_argv``, ``python3 -c`` by default, so each call is a fresh
        process and only what it prints comes back, trailing newline
        included: use ``print(...)``, not a bare expression.

        Raises `RuntimeError` when the snippet fails, with its stderr.

        Example
        -------
        >>> sb.eval("print(2 + 2)")
        '4\\n'
        """
        resp = self.controller.eval_code(
            self.id, code, sandbox_token=self.token, timeout_secs=self.timeout
        )
        if resp.get("error") is not None:
            raise RuntimeError(
                f"sandstorm eval (exit {resp.get('exit_code')}): {resp['error']}"
            )
        return resp.get("result", "")

    def ping(self) -> dict:
        """Ask the master about this sandbox.

        Returns ``{ok, tier, guest_addr, latency_ms}``. ``ok`` says whether
        something accepts connections on the sandbox's port, so it stays
        false in a running sandbox that serves nothing.
        """
        return self.controller.ping_sandbox(self.id, self.token)

    @classmethod
    def create(cls, *args: Any, **kwargs: Any) -> "Sandbox":
        """Alias for `Sandbox(...)` matching E2B's `Sandbox.create()` style."""
        return cls(*args, **kwargs)

    @classmethod
    def connect(
        cls,
        sandbox_id: str,
        token: str,
        *,
        timeout: int = 30,
        start_timeout: float = 60.0,
        controller: Optional[Controller] = None,
    ) -> "Sandbox":
        """Wrap a sandbox that already exists and wait for it to be running.

        `token` is the ``token`` field of the response that created it. This
        is how to drive what `Controller.spawn_sandboxes` made. The sandbox is
        not created here but `kill` and a ``with`` block still terminate it.
        """
        sb = cls.__new__(cls)
        sb._bind(controller or Controller(), timeout, {"id": sandbox_id, "token": token})
        sb._wait_until_running(start_timeout)
        return sb

    def kill(self) -> None:
        """Terminate the sandbox. Safe to call twice."""
        if self._killed:
            return
        self.controller.kill_sandbox(self.id)
        self._killed = True

    # ----- context manager ------------------------------------------------

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.kill()

    # ----- internals ------------------------------------------------------

    def _bind(self, controller: Controller, timeout: int, info: dict) -> None:
        self.controller = controller
        self.timeout = timeout
        self.info = info
        self.id = info["id"]
        # Only the create response carries this, and nothing can reach into
        # the sandbox without it.
        self.token = info["token"]
        self.commands = _CommandsProxy(self)
        self._killed = False

    def _wait_until_running(self, start_timeout: float) -> None:
        """Block until a node reports this sandbox running.

        The master answers a create before any node has acted on it, and an
        exec that arrives first fails. A node reports the address only for a
        sandbox with processes, so that is the signal. `ping` is not: its
        ``ok`` also needs something listening inside.

        A program that fails to load never gets that far: the node removes it
        and the master quarantines it, which raises here. One that starts and
        then exits has reported an address by then, so this returns, and the
        next command fails while the node restarts it.
        """
        deadline = time.monotonic() + start_timeout
        pause = 0.05
        while True:
            seen = self.controller.get_sandbox(self.id)
            if seen.get("ip"):
                self.info = {**self.info, **seen}
                return
            reason = seen.get("last_error") or "no reason recorded"
            if seen.get("error_attempts", 0) >= _MAX_START_FAILURES:
                raise RuntimeError(f"sandbox {self.id} failed to start: {reason}")
            if time.monotonic() + pause > deadline:
                raise RuntimeError(
                    f"sandbox {self.id} did not start within {start_timeout}s; "
                    f"last error: {reason}"
                )
            time.sleep(pause)
            pause = min(pause * 2, 1.0)

    def _discard(self) -> None:
        """Remove a sandbox that never came up, keeping the original error."""
        try:
            self.kill()
        except (ControllerError, OSError):
            pass
