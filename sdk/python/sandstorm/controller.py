"""HTTP client for a Sandstorm master's REST API.

The `Controller` class wraps `/v1/snapshots` and `/v1/sandboxes` endpoints
(docs/API.md) so Python agent code can manage snapshots, fork sandboxes,
branch running sandboxes, and tear things down.

`sandstorm.Sandbox` is built on this class: it holds one sandbox and its
token, which is the easier way to drive a single sandbox. Use Controller
directly for the fleet-wide calls, fan-out and branching.

Example
-------

>>> from sandstorm import Controller, Sandbox
>>> c = Controller()  # $SANDSTORM_URL, $SANDSTORM_TOKEN and $SANDSTORM_CA
>>> [s["tag"] for s in c.list_snapshots()]
['pyagent']
>>> children = c.spawn_sandboxes("pyagent", n=1, per_child_netns=True)
>>> sb_id = children[0]["id"]
>>> sb = Sandbox.connect(sb_id, children[0]["token"], controller=c)
>>> branch = c.branch_sandbox(sb_id, tag="checkpoint-1")
>>> branch["tag"]
'checkpoint-1'
>>> branch["branched_from"]
'sb-...'
>>> grandchildren = c.spawn_sandboxes(branch["tag"], n=5)
>>> c.kill_sandbox(sb_id)
"""

from __future__ import annotations

import json
import os
import ssl
import urllib.error
import urllib.request
from typing import Any, Optional

#: The master answers 504 when a node takes longer than 180 s to branch.
_BRANCH_TIMEOUT = 185.0

#: Header carrying a sandbox's own access token (RFC 16.5 layer two).
SANDBOX_TOKEN_HEADER = "X-Sandstorm-Sandbox-Token"


class ControllerError(RuntimeError):
    """Raised on non-2xx responses from the master.

    Carries the HTTP status code and the master's parsed error body
    (when it returned JSON). Inspect ``status`` and ``body`` to
    distinguish 404 (sandbox/snapshot missing) from 409 (tag
    collision) from 500 (internal).
    """

    def __init__(self, status: int, body: Any, url: str) -> None:
        self.status = status
        self.body = body
        self.url = url
        snippet = body if isinstance(body, str) else json.dumps(body)
        super().__init__(f"controller {url}: HTTP {status}: {snippet}")


class Controller:
    """Client for a Sandstorm master's REST API.

    Parameters
    ----------
    base_url:
        Master base URL. Defaults to ``$SANDSTORM_URL`` then
        ``https://127.0.0.1:8080``, where a master listens by default.
    token:
        The master's API token. Defaults to ``$SANDSTORM_TOKEN``.
    timeout:
        Per-request timeout in seconds. Exec, eval and branch wait longer
        when the master may legitimately take longer.
    ca_file:
        PEM bundle to verify an ``https://`` master against. Defaults to
        ``$SANDSTORM_CA``.

        A v0.6 master serves TLS by default under a CA it generates
        itself, so the public trust store cannot verify it and every
        call fails with ``CERTIFICATE_VERIFY_FAILED``. Point this at the
        master's ``pki/ca.pem``, or fetch it -- the master serves it
        unauthenticated for exactly this reason::

            curl -k https://master:8080/ca.pem -o ca.pem

        and check it against the fingerprint in a registration token
        before you trust it. There is deliberately no way to turn
        verification off: an unverified control plane carries the API
        token and every command run inside a sandbox.
    """

    def __init__(
        self,
        base_url: Optional[str] = None,
        token: Optional[str] = None,
        timeout: float = 60.0,
        ca_file: Optional[str] = None,
    ) -> None:
        self.base_url = (
            base_url
            or os.environ.get("SANDSTORM_URL")
            or "https://127.0.0.1:8080"
        ).rstrip("/")
        self.token = token if token is not None else os.environ.get("SANDSTORM_TOKEN")
        self.timeout = timeout
        self.ca_file = ca_file if ca_file is not None else os.environ.get("SANDSTORM_CA")
        # Built once: loading a PEM per request is wasted work, and the
        # context is what carries the private CA.
        self._ssl_context: Optional[ssl.SSLContext] = None
        if self.base_url.startswith("https://"):
            # cafile= replaces the default trust store rather than adding
            # to it, which is what we want: the master's CA is the only
            # issuer that should be able to sign for it.
            self._ssl_context = ssl.create_default_context(cafile=self.ca_file)

    # --- templates ------------------------------------------------

    def list_templates(self) -> list[str]:
        """``GET /v1/templates`` — the names of the registered templates."""
        return self._request("GET", "/v1/templates")

    # --- snapshots ------------------------------------------------

    def list_snapshots(self) -> list[dict]:
        """``GET /v1/snapshots`` — every snapshot known to the daemon."""
        return self._request("GET", "/v1/snapshots")

    def delete_snapshot(self, tag: str) -> None:
        """``DELETE /v1/snapshots/:tag`` — drop both registry and disk files."""
        self._request("DELETE", f"/v1/snapshots/{tag}")

    # --- sandboxes ------------------------------------------------

    def create_sandbox(
        self,
        template: Optional[str] = None,
        snapshot_tag: Optional[str] = None,
        memory_limit_mib: Optional[int] = None,
    ) -> dict:
        """``POST /v1/sandboxes`` — one sandbox from a template or a tag.

        Give exactly one of ``template`` (start cold) and ``snapshot_tag``
        (fork). Returns a SandboxInfo dict, the only response that carries
        the sandbox's ``token``. The master answers before any node has
        started it, so ``ip`` is absent until one reports it running.
        """
        body: dict[str, Any] = {}
        if template is not None:
            body["template"] = template
        if snapshot_tag is not None:
            body["snapshot_tag"] = snapshot_tag
        if memory_limit_mib is not None:
            body["memory_limit_mib"] = memory_limit_mib
        return self._request("POST", "/v1/sandboxes", body)

    def spawn_sandboxes(
        self,
        snapshot_tag: str,
        n: int = 1,
        per_child_netns: bool = False,
        memory_limit_mib: Optional[int] = None,
    ) -> list[dict]:
        """``POST /v1/sandboxes`` — fork N children from a snapshot tag.

        ``per_child_netns`` is accepted and ignored: every sandbox has its own
        network namespace. Each child carries its ``token`` this one time.
        ``ip`` and ``guest_addr`` are absent until a node reports it running.
        """
        body: dict[str, Any] = {
            "snapshot_tag": snapshot_tag,
            "n": n,
            "per_child_netns": per_child_netns,
        }
        if memory_limit_mib is not None:
            body["memory_limit_mib"] = memory_limit_mib
        return self._request("POST", "/v1/sandboxes", body)

    def list_sandboxes(self) -> list[dict]:
        """``GET /v1/sandboxes`` — every live sandbox the daemon tracks."""
        return self._request("GET", "/v1/sandboxes")

    def get_sandbox(self, sandbox_id: str) -> dict:
        """``GET /v1/sandboxes/:id`` — one sandbox's metadata."""
        return self._request("GET", f"/v1/sandboxes/{sandbox_id}")

    def kill_sandbox(self, sandbox_id: str) -> None:
        """``DELETE /v1/sandboxes/:id`` — terminate one sandbox."""
        self._request("DELETE", f"/v1/sandboxes/{sandbox_id}")

    def touch_sandbox(self, sandbox_id: str) -> dict:
        """``POST /v1/sandboxes/:id/touch`` — mark it used, which brings it back to hot."""
        return self._request("POST", f"/v1/sandboxes/{sandbox_id}/touch")

    def branch_sandbox(self, sandbox_id: str, tag: str) -> dict:
        """``POST /v1/sandboxes/:id/branch`` — checkpoint it into a new tag.

        ``tag`` is a plain name: no slashes, no leading dots. The parent keeps
        running after a short pause, and this returns once the node has
        written the image. The sandbox must be hot: the master answers 409
        otherwise, and :meth:`touch_sandbox` wakes it.

        Returns a Snapshot dict; pass its ``tag`` to ``spawn_sandboxes`` to
        fork children from the branch.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/branch",
            {"tag": tag},
            timeout=max(self.timeout, _BRANCH_TIMEOUT),
        )

    def exec_command(
        self,
        sandbox_id: str,
        args: list[str],
        timeout_secs: int = 30,
        sandbox_token: Optional[str] = None,
    ) -> dict:
        """``POST /v1/sandboxes/:id/exec`` — run a subprocess in the sandbox.

        Returns ``{stdout, stderr, exit_code}``.

        ``sandbox_token`` is the ``token`` field of the response that created
        the sandbox. From v0.6 the daemon requires it on every call that
        reaches into a sandbox: the bearer token says you may talk to the
        API, this says you may enter *this* sandbox. Omitting it gets a 403.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/exec",
            {"args": args, "timeout_secs": timeout_secs},
            sandbox_token=sandbox_token,
            timeout=self._command_timeout(timeout_secs),
        )

    def eval_code(
        self,
        sandbox_id: str,
        code: str,
        sandbox_token: Optional[str] = None,
        timeout_secs: int = 30,
    ) -> dict:
        """``POST /v1/sandboxes/:id/eval`` — run a snippet in the sandbox.

        Returns ``{result, error, exit_code}``. See ``exec_command`` for
        ``sandbox_token``.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/eval",
            {"code": code, "timeout_secs": timeout_secs},
            sandbox_token=sandbox_token,
            timeout=self._command_timeout(timeout_secs),
        )

    def ping_sandbox(self, sandbox_id: str, sandbox_token: Optional[str] = None) -> dict:
        """``POST /v1/sandboxes/:id/ping`` — round-trip to the guest agent.

        See ``exec_command`` for ``sandbox_token``.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/ping",
            sandbox_token=sandbox_token,
        )

    # --- internals ------------------------------------------------

    def _command_timeout(self, timeout_secs: int) -> float:
        """How long to wait for a command the sandbox may run for this long.

        The master gives the node ``timeout_secs`` plus 20 s and then answers
        504, so waiting a little longer gets its answer rather than our own.
        """
        return max(self.timeout, timeout_secs + 25)

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[dict] = None,
        sandbox_token: Optional[str] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if body is not None else {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if sandbox_token:
            # A header, not the path or query: this is a secret, and a query
            # string ends up in logs and in Referer.
            headers[SANDBOX_TOKEN_HEADER] = sandbox_token
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(
                req, timeout=timeout or self.timeout, context=self._ssl_context
            ) as resp:
                raw = resp.read()
                if not raw:
                    return None
                return json.loads(raw)
        except urllib.error.HTTPError as e:
            raw = e.read()
            parsed: Any
            try:
                parsed = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                parsed = raw.decode(errors="replace")
            raise ControllerError(e.code, parsed, url) from e
