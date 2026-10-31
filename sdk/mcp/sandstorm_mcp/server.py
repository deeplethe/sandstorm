"""MCP server for Sandstorm sandboxes.

Wraps a Sandstorm master's HTTP API (docs/API.md) as MCP tools, so any
MCP-aware client (Claude Desktop, Claude Code, etc.) can use sandstorm as
a code-execution backend.

Config via environment:
- SANDSTORM_URL    — master base URL (default: https://127.0.0.1:8080)
- SANDSTORM_TOKEN  — the master's API token
- SANDSTORM_CA     — PEM bundle to verify an https:// master, usually its
                     pki/ca.pem. A master signs its certificate with a CA of
                     its own, so the public trust store cannot verify it.
                     There is deliberately no way to turn verification off:
                     an unverified control plane carries the API token and
                     every command run inside a sandbox.

Run:
    sandstorm-mcp                       # stdio transport, the MCP default

The server is stateless: each tool call hits the master fresh. The master
owns sandbox state.
"""

from __future__ import annotations

import os
import ssl
import time
from typing import Any
from urllib.parse import quote

import httpx
from mcp.server.fastmcp import FastMCP

SANDSTORM_URL = os.environ.get("SANDSTORM_URL", "https://127.0.0.1:8080").rstrip("/")
SANDSTORM_TOKEN = os.environ.get("SANDSTORM_TOKEN", "").strip()
SANDSTORM_CA = os.environ.get("SANDSTORM_CA") or None
HTTP_TIMEOUT = float(os.environ.get("SANDSTORM_HTTP_TIMEOUT", "60"))

# The master answers 504 when a node takes longer than 180 s to branch.
BRANCH_TIMEOUT = 185.0
# The master stops retrying a sandbox that has failed to start this many
# times (MAX_FAILURES in master/api.rs), so waiting longer is futile.
MAX_START_FAILURES = 5

mcp = FastMCP("sandstorm")


def _ssl_context() -> ssl.SSLContext | None:
    if not SANDSTORM_URL.startswith("https://"):
        return None
    # As the Python SDK's Controller. A context rather than verify=<path>,
    # which httpx deprecated, and cafile= replaces the default trust store
    # rather than adding to it, which is what we want: the master's CA is
    # the only issuer that should be able to sign for it.
    return ssl.create_default_context(cafile=SANDSTORM_CA)


# Built once: loading a PEM per tool call is wasted work, and wait_for_text
# opens a client per poll. A bad SANDSTORM_CA stops the server here.
SSL_CONTEXT = _ssl_context()


def _headers(sandbox_token: str | None = None) -> dict[str, str]:
    h = {"Content-Type": "application/json"}
    if SANDSTORM_TOKEN:
        h["Authorization"] = f"Bearer {SANDSTORM_TOKEN}"
    if sandbox_token is not None:
        h["x-sandstorm-sandbox-token"] = sandbox_token
    return h


def _client(sandbox_token: str | None = None, timeout: float = HTTP_TIMEOUT) -> httpx.Client:
    return httpx.Client(
        base_url=SANDSTORM_URL,
        headers=_headers(sandbox_token),
        timeout=max(timeout, HTTP_TIMEOUT),
        verify=SSL_CONTEXT if SSL_CONTEXT is not None else True,
    )


def _command_timeout(timeout_secs: float) -> float:
    """How long to wait for a command the sandbox may run for this long.

    The master gives the node timeout_secs plus 20 s and then answers 504, so
    waiting a little longer gets its answer rather than our own.
    """
    return timeout_secs + 25


def _wait_until_running(c: httpx.Client, sandbox_id: str, deadline: float) -> dict[str, Any]:
    """Block until a node reports this sandbox running, and return it.

    The master answers a create before any node has acted on it, and an exec
    that arrives first fails. A node reports the address only for a sandbox
    with processes, so that is the signal. ping is not: its ok also needs
    something listening inside.
    """
    pause = 0.05
    while True:
        r = c.get(f"/v1/sandboxes/{sandbox_id}")
        r.raise_for_status()
        seen = r.json()
        if seen.get("ip"):
            return seen
        reason = seen.get("last_error") or "no reason recorded"
        if seen.get("error_attempts", 0) >= MAX_START_FAILURES:
            raise RuntimeError(f"sandbox {sandbox_id} failed to start: {reason}")
        if time.monotonic() + pause > deadline:
            raise RuntimeError(
                f"sandbox {sandbox_id} did not start in time; last error: {reason}"
            )
        time.sleep(pause)
        pause = min(pause * 2, 1.0)


@mcp.tool()
def list_templates() -> list[str]:
    """List the registered templates, which spawn_sandboxes can start from."""
    with _client() as c:
        r = c.get("/v1/templates")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def list_snapshots() -> list[dict[str, Any]]:
    """List snapshot tags, which spawn_sandboxes can fork from.

    A tag is a checkpoint made by branch_sandbox, so a new deployment has
    none. Returns Snapshot objects: tag, node_id, template, from_sandbox,
    dir, size_bytes, created_at.
    """
    with _client() as c:
        r = c.get("/v1/snapshots")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def delete_snapshot(tag: str) -> dict[str, Any]:
    """Delete a snapshot tag. Refused with 409 while sandboxes are forked from it."""
    with _client() as c:
        r = c.delete(f"/v1/snapshots/{quote(tag, safe='')}")
        r.raise_for_status()
        return {"tag": tag, "deleted": True}


@mcp.tool()
def spawn_sandboxes(
    template: str | None = None,
    snapshot_tag: str | None = None,
    n: int = 1,
    memory_limit_mib: int | None = None,
    start_timeout_secs: float = 60,
) -> list[dict[str, Any]]:
    """Start N sandboxes from a template, or fork them from a snapshot tag.

    Waits until each one is running, so it can be used straight away.

    Args:
        template: Template to start from (see list_templates). Give this or
            snapshot_tag, not both.
        snapshot_tag: Tag to fork from (see list_snapshots).
        n: Number of sandboxes, 1..256.
        memory_limit_mib: Memory ceiling for each, overriding the template's.
            Leave unset to use the template's.
        start_timeout_secs: How long to wait for all of them to start.

    Returns the Sandbox objects (one per child). Retain each one's token for
    exec_command, eval_code, ping_sandbox, and wait_for_text: only this
    response carries it. If any fails to start, all are removed and this
    raises with the reason.
    """
    if (template is None) == (snapshot_tag is None):
        raise ValueError("give a template or a snapshot_tag, not both or neither")
    body: dict[str, Any] = {"n": n}
    if template is not None:
        body["template"] = template
    if snapshot_tag is not None:
        body["snapshot_tag"] = snapshot_tag
    if memory_limit_mib is not None:
        body["memory_limit_mib"] = memory_limit_mib
    with _client() as c:
        r = c.post("/v1/sandboxes", json=body)
        r.raise_for_status()
        created = r.json()
        deadline = time.monotonic() + start_timeout_secs
        try:
            # The read has no token, so keep the create response's.
            return [{**s, **_wait_until_running(c, s["id"], deadline)} for s in created]
        except BaseException:
            # Their tokens exist only here, so nothing could reach them later.
            for s in created:
                try:
                    c.delete(f"/v1/sandboxes/{s['id']}")
                except httpx.HTTPError:
                    pass  # Keep the original error.
            raise


@mcp.tool()
def touch_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Mark a sandbox used, which brings it back to hot.

    branch_sandbox needs a hot sandbox and answers 409 otherwise, and an idle
    one is moved to a colder tier.
    """
    with _client() as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/touch")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def branch_sandbox(sandbox_id: str, tag: str) -> dict[str, Any]:
    """Checkpoint a running sandbox into a new snapshot tag.

    The parent keeps running, after a short pause. Any later spawn_sandboxes
    call can use the tag as its snapshot_tag, to fan out N children that all
    inherit the source's exact state at branch time.

    This is sandstorm's core primitive.

    Args:
        sandbox_id: Id of the source sandbox (see list_sandboxes). It must be
            hot; touch_sandbox wakes it.
        tag: Name for the new snapshot: no slashes, no leading dots.

    Returns a Snapshot: tag, node_id, template, from_sandbox, dir,
    size_bytes, created_at.
    """
    with _client(timeout=BRANCH_TIMEOUT) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/branch", json={"tag": tag})
        r.raise_for_status()
        return r.json()


@mcp.tool()
def wait_for_text(
    sandbox_id: str,
    path: str,
    marker: str,
    timeout_secs: float = 60,
    poll_interval_ms: int = 200,
    sandbox_token: str | None = None,
) -> dict[str, Any]:
    """Poll a file inside a sandbox until it contains a marker string.

    Common pattern: agent writes its progress to a stdout log inside
    the guest; the orchestrator (you, the MCP client) polls until a
    marker like "READY_TO_BRANCH" appears, then triggers branch.
    Avoids busy-waiting from outside the master's exec round-trip.

    Args:
        sandbox_id: Target sandbox.
        path: Absolute path inside the guest of the file to poll.
        marker: Substring to wait for. Returned as-is when found.
        timeout_secs: Max wall-clock seconds to wait. Default 60.
        poll_interval_ms: How often to re-check. Default 200 ms.
        sandbox_token: Token from the creation response; required by v0.6.

    Returns: {"found": bool, "elapsed_ms": int, "last_excerpt": str}.
    When found is false, last_excerpt contains the last ~256 bytes
    of the file so you can see what's actually being written.
    """
    deadline = time.monotonic() + timeout_secs
    last_excerpt = ""
    started = time.monotonic()
    while time.monotonic() < deadline:
        # Use exec_command for one-shot read; the master's exec returns
        # stdout+exit, and a short tail of the target file is cheap.
        with _client(sandbox_token) as c:
            r = c.post(
                f"/v1/sandboxes/{sandbox_id}/exec",
                json={
                    "args": ["sh", "-c", f"tail -c 4096 {path} 2>/dev/null || true"],
                    "timeout_secs": 5,
                },
            )
            r.raise_for_status()
            last_excerpt = r.json().get("stdout", "") or ""
            if marker in last_excerpt:
                return {
                    "found": True,
                    "elapsed_ms": int((time.monotonic() - started) * 1000),
                    "last_excerpt": last_excerpt[-256:],
                }
        time.sleep(poll_interval_ms / 1000)
    return {
        "found": False,
        "elapsed_ms": int((time.monotonic() - started) * 1000),
        "last_excerpt": last_excerpt[-256:],
    }


@mcp.tool()
def list_sandboxes() -> list[dict[str, Any]]:
    """List every sandbox, on whichever tier it is. Tokens are not included."""
    with _client() as c:
        r = c.get("/v1/sandboxes")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def get_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Fetch metadata about one sandbox by id. Its token is not included."""
    with _client() as c:
        r = c.get(f"/v1/sandboxes/{sandbox_id}")
        r.raise_for_status()
        return r.json()


@mcp.tool()
def kill_sandbox(sandbox_id: str) -> dict[str, Any]:
    """Delete one sandbox, on whichever tier it is."""
    with _client() as c:
        r = c.delete(f"/v1/sandboxes/{sandbox_id}")
        r.raise_for_status()
        return {"id": sandbox_id, "killed": True}


@mcp.tool()
def exec_command(
    sandbox_id: str,
    args: list[str],
    timeout_secs: int = 30,
    sandbox_token: str | None = None,
) -> dict[str, Any]:
    """Run a subprocess inside a sandbox.

    Example:
        exec_command("sb-abc-0000", ["python3", "-c", "print(2+2)"],
                     sandbox_token="<token-from-creation>")

    sandbox_token: Token from the creation response; required by v0.6.

    Returns: {stdout, stderr, exit_code}.
    """
    body = {"args": args, "timeout_secs": timeout_secs}
    with _client(sandbox_token, _command_timeout(timeout_secs)) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/exec", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def eval_code(
    sandbox_id: str,
    code: str,
    sandbox_token: str | None = None,
    timeout_secs: int = 30,
) -> dict[str, Any]:
    """Run a code snippet in a sandbox and return what it printed.

    The snippet runs with the template's eval_argv, python3 -c by default, in
    a fresh process. Only printed output comes back, so use print(2+2), not a
    bare expression.

    sandbox_token: Token from the creation response; required by v0.6.

    Returns: {result, exit_code}, or {error, exit_code} if it failed.
    """
    body = {"code": code, "timeout_secs": timeout_secs}
    with _client(sandbox_token, _command_timeout(timeout_secs)) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/eval", json=body)
        r.raise_for_status()
        return r.json()


@mcp.tool()
def ping_sandbox(sandbox_id: str, sandbox_token: str | None = None) -> dict[str, Any]:
    """Check a sandbox answers.

    sandbox_token: Token from the creation response; required by v0.6.

    Returns: {ok, tier, guest_addr, latency_ms}. ok says whether something
    accepts connections on the sandbox's port, so it stays false in a running
    sandbox that serves nothing.
    """
    with _client(sandbox_token) as c:
        r = c.post(f"/v1/sandboxes/{sandbox_id}/ping")
        r.raise_for_status()
        return r.json()


def main() -> None:
    """Entrypoint for the `sandstorm-mcp` console script."""
    mcp.run()


if __name__ == "__main__":
    main()
