#!/usr/bin/env python3
"""sandstorm guest agent — runs as PID 1, warms state into memory, accepts
commands from the host via TCP on port 8888.

Protocol: each request is one JSON object terminated by '\n'. Response is
one JSON object terminated by '\n'. Multiple requests on one connection
are allowed.

Actions:
  {"action": "ping"}
    → {"pong": true, "numpy_version": "1.26.4", "pid": 1}

  {"action": "exec", "args": ["python3", "-c", "print(1+1)"], "timeout": 10}
    → {"stdout": "2\n", "stderr": "", "exit_code": 0}

  {"action": "set_time", "unix": 1793289195.25}
    → {"ok": true}

  {"action": "eval", "code": "1 + numpy.zeros(3).sum()"}
    → {"result": "1.0", "exit_code": 0}

  {"action": "stream", "args": ["bash"], "pty": true}
    → {"stream": "started", "pid": 1234, "pty": true}
      then {"out": "$ "} chunks as output arrives
      client sends {"in": "ls\n"} to write to stdin
      client sends {"action": "stop"} to terminate
      final message: {"exit_code": 0}

`eval` semantics depend on the recipe. By default the code is evaluated
as a Python expression against the agent's interpreter (numpy is in
scope when available). If /etc/sandstorm-recipe.env declares
`SANDSTORM_AGENT_LANG=node`, the same action routes to a warm-up subprocess
(launched per `SANDSTORM_WARMUP_CMD`) over a line-JSON bridge, so a rootfs can
evaluate JS against a warmed runtime.

This file is copied to / in a Firecracker rootfs, then launched by
/sandstorm-init.sh after the kernel finishes mounting
/proc /sys /dev.
"""

import json
import os

# ---------------------------------------------------------------------------
# Container environment
#
# sandstorm-agent runs as PID 1 inside the sandbox VM.  Its own environment is
# inherited from the kernel boot / init script, which may leak host PATH
# values (e.g. /opt/homebrew/bin, /Users/.../.cargo/bin) into the guest.
# Every exec/stream command would then resolve binaries against the host
# PATH instead of the container's, causing "command not found" or silent
# wrong-binary usage.
#
# To fix this we read /etc/environment (the standard Linux system-wide
# env file, written by pam_env and most Docker/base images) at startup
# and use its PATH as the default for all subprocess calls.  If the file
# is missing or has no PATH, we fall back to a sane Linux default.
# ---------------------------------------------------------------------------

_DEFAULT_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"


def _load_container_env(path: str = "/etc/environment") -> dict:
    """Parse /etc/environment for KEY=VALUE pairs (pam_env format).

    Unlike shell scripts, /etc/environment has no `export` keyword and
    no command substitution — just simple KEY=VALUE lines, optionally
    quoted.  This is the canonical source of system-wide environment
    defaults on Linux.
    """
    env: dict = {}
    try:
        with open(path) as f:
            for raw in f:
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                key, sep, val = s.partition("=")
                if not sep:
                    continue
                key = key.strip()
                val = val.strip()
                if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                    val = val[1:-1]
                env[key] = val
    except OSError:
        pass
    return env


# Base environment for all subprocess calls: start from the agent's own
# environ (to keep HOME, USER, etc.) but override PATH with the
# container's value.  Callers can still override individual vars per
# command — see _subprocess_env() below.
_CONTAINER_ENV = _load_container_env()
_CONTAINER_PATH = _CONTAINER_ENV.get("PATH") or _DEFAULT_PATH
GUEST_ENV = dict(os.environ)
GUEST_ENV["PATH"] = _CONTAINER_PATH
# Carry over any other /etc/environment vars not already in environ.
for _k, _v in _CONTAINER_ENV.items():
    if _k not in GUEST_ENV:
        GUEST_ENV[_k] = _v


def _app_from_cmdline(cmdline: str) -> "dict | None":
    """The template's program, as the Firecracker backend passes it:
    `sandstorm.app=<hex of {"args", "env", "cwd"}>` on the kernel command line.
    None when there is none, which is every guest the v0.5 CLI booted."""
    for word in cmdline.split():
        if word.startswith("sandstorm.app="):
            try:
                app = json.loads(bytes.fromhex(word.split("=", 1)[1]))
            except ValueError as e:
                print(f"sandstorm: ignoring an unreadable sandstorm.app: {e}", flush=True)
                return None
            return app if isinstance(app, dict) and app.get("args") else None
    return None


def _which(name: str, path: str) -> "str | None":
    if "/" in name:
        return name
    for d in path.split(":"):
        full = os.path.join(d or ".", name)
        if os.path.isfile(full) and os.access(full, os.X_OK):
            return full
    return None


def _start_app() -> None:
    """Start the template's program beside the agent, in its own session so
    a stop aimed at it does not reach PID 1. Its output goes to the console,
    which the host keeps in the sandbox's log.

    Called before the agent imports anything it does not need for this, and
    with posix_spawn rather than subprocess: the program is what a caller is
    waiting for, and the agent's own start (pty, socket, threading and the
    rest) used to come first, on the same single vCPU."""
    try:
        with open("/proc/cmdline") as f:
            app = _app_from_cmdline(f.read())
    except OSError:
        return
    if app is None:
        return
    env = dict(GUEST_ENV)
    for kv in app.get("env") or []:
        k, _, v = kv.partition("=")
        env[k] = v
    args = app["args"]
    exe = _which(args[0], env.get("PATH", _DEFAULT_PATH))
    if exe is None:
        print(f"sandstorm: could not start the template's program: {args[0]} not found", flush=True)
        return
    out = "/dev/console" if os.path.exists("/dev/console") else "/dev/null"
    actions = [
        (os.POSIX_SPAWN_OPEN, 0, "/dev/null", os.O_RDONLY, 0),
        (os.POSIX_SPAWN_OPEN, 1, out, os.O_WRONLY, 0),
        (os.POSIX_SPAWN_DUP2, 1, 2),
    ]
    # posix_spawn takes no working directory, so PID 1 lends it its own.
    here = os.getcwd()
    try:
        os.chdir(app.get("cwd") or "/")
        pid = os.posix_spawn(exe, args, env, file_actions=actions, setsid=True)
        print(f"sandstorm: started the template's program, pid {pid}", flush=True)
    except Exception as e:
        print(f"sandstorm: could not start the template's program: {e}", flush=True)
    finally:
        os.chdir(here)


if __name__ == "__main__":
    _start_app()

import itertools
import pty
import select
import shlex
import signal
import socket
import subprocess
import sys
import threading
import time
import traceback
from typing import Optional



def _subprocess_env(caller_env: dict | None = None) -> dict:
    """Build the env dict for a subprocess call.

    If the caller provides an env, it is merged on top of GUEST_ENV so
    that PATH (and other container defaults) are still present unless
    the caller explicitly overrides them.  If the caller provides
    nothing, a copy of GUEST_ENV is returned.
    """
    if caller_env is None:
        return dict(GUEST_ENV)
    merged = dict(GUEST_ENV)
    merged.update(caller_env)
    return merged

# numpy for `eval`, imported the first time it is asked for. It used to be
# imported here, as a demo of forking from warmed state, which held the
# template's program back until PID 1 had loaded it: about 300 ms of every
# cold Firecracker start on a 2013 Xeon. The version is read from the
# package metadata instead, which imports nothing.
_np = None
try:
    import importlib.metadata as _md

    NUMPY_VERSION = _md.version("numpy")
except Exception:
    NUMPY_VERSION = "not-installed"


def _numpy():
    """numpy, imported on first use; None where the image has none."""
    global _np
    if _np is None and NUMPY_VERSION != "not-installed":
        try:
            import numpy as np

            _np = np
        except ImportError:
            pass
    return _np


def _load_recipe_env(path: str = "/etc/sandstorm-recipe.env") -> dict:
    """Parse a minimal KEY=VALUE env file. Supports quoted values and # comments.

    Recipes drop this file into the rootfs to declare per-recipe agent
    behaviour without code changes to sandstorm-agent itself. Currently
    consumed keys:

      SANDSTORM_WARMUP_CMD   shell-tokenised command to spawn before serving
      SANDSTORM_AGENT_LANG   "node" routes the `eval` action to the warmup
                         subprocess via a stdin/stdout JSON bridge.
                         Anything else (or absent) keeps the default
                         Python eval path.
    """
    env: dict = {}
    try:
        with open(path) as f:
            for raw in f:
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                key, sep, val = s.partition("=")
                if not sep:
                    continue
                key = key.strip()
                val = val.strip()
                if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                    val = val[1:-1]
                env[key] = val
    except FileNotFoundError:
        pass
    return env


RECIPE_ENV = _load_recipe_env()
# Allow process env vars to override the recipe file. Useful for dev
# smoke tests on the host before baking a real rootfs, and for kernel
# cmdline-injected overrides at boot.
for _override_key in ("SANDSTORM_WARMUP_CMD", "SANDSTORM_AGENT_LANG"):
    if _override_key in os.environ:
        RECIPE_ENV[_override_key] = os.environ[_override_key]
AGENT_LANG = RECIPE_ENV.get("SANDSTORM_AGENT_LANG", "python")

# Warm-up subprocess state. None on default Python recipes.
_warmup_proc: "subprocess.Popen | None" = None
_warmup_lock = threading.Lock()
_warmup_ready = False
_req_id_counter = itertools.count(1)


def _drain_stderr(proc: subprocess.Popen) -> None:
    """Forward warmup subprocess stderr to agent stdout for visibility."""
    assert proc.stderr is not None
    for raw in iter(proc.stderr.readline, b""):
        sys.stdout.buffer.write(b"sandstorm-warmup: " + raw)
        sys.stdout.flush()


def _kill_process_group(proc: subprocess.Popen, grace: float = 2.0) -> None:
    """Terminate a stream command and every descendant via its process group.

    Stream commands are spawned with start_new_session=True, which makes
    the child the leader of a fresh process group (pgid == proc.pid).
    Signalling the NEGATIVE pgid reaches the shell AND all descendants it
    spawned, so e.g. `sh -c "sleep 30"` cannot leave `sleep` running after
    a stop or disconnect.
    """
    pgid = proc.pid

    def _group_gone() -> bool:
        try:
            os.killpg(pgid, 0)
        except (ProcessLookupError, PermissionError):
            return True
        return False

    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            break
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            if _group_gone():
                break
            time.sleep(0.05)
    # Reap the leader so it doesn't linger as a zombie.
    try:
        proc.wait(timeout=1)
    except Exception:
        pass


def _start_warmup() -> None:
    """If SANDSTORM_WARMUP_CMD is set, spawn it and wait for the ready handshake.

    The warmup process speaks a line-JSON protocol on stdin/stdout. First
    line on stdout MUST be {"ready": true} once the workload (e.g.
    headless Chromium) has finished initialising; after that, the agent
    can send {"id", "code"} requests and read replies.
    """
    global _warmup_proc, _warmup_ready
    cmd = RECIPE_ENV.get("SANDSTORM_WARMUP_CMD")
    if not cmd:
        return
    print(f"sandstorm: starting warmup (lang={AGENT_LANG}): {cmd}", flush=True)
    try:
        _warmup_proc = subprocess.Popen(
            shlex.split(cmd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            env=GUEST_ENV,
        )
    except Exception as e:
        print(f"sandstorm: failed to spawn warmup: {e}", flush=True)
        return

    threading.Thread(target=_drain_stderr, args=(_warmup_proc,), daemon=True).start()

    # Block on the ready handshake. Warmup may take seconds (Chromium
    # launch, model load, etc.); the snapshot --boot-wait-secs flag
    # gives this room to complete.
    ready_line = _warmup_proc.stdout.readline()
    if not ready_line:
        rc = _warmup_proc.poll()
        print(f"sandstorm: warmup exited before ready (rc={rc})", flush=True)
        return
    try:
        msg = json.loads(ready_line)
    except Exception as e:
        print(f"sandstorm: warmup ready parse error: {e}, raw={ready_line!r}", flush=True)
        return
    if msg.get("ready"):
        _warmup_ready = True
        print("sandstorm: warmup ready", flush=True)
    else:
        print(f"sandstorm: warmup signalled: {msg}", flush=True)


def _bridge_eval(code: str) -> dict:
    """Route an `eval` action to the warmup subprocess and return the reply.

    Serialised by _warmup_lock so concurrent connections within one VM
    don't interleave on the shared stdin/stdout. Cross-VM concurrency
    is unaffected since each child VM has its own agent + warmup pair.
    """
    if not _warmup_ready or _warmup_proc is None:
        return {"error": "warmup not ready", "exit_code": 1}
    req_id = str(next(_req_id_counter))
    payload = json.dumps({"id": req_id, "code": code}).encode() + b"\n"
    with _warmup_lock:
        try:
            _warmup_proc.stdin.write(payload)
            _warmup_proc.stdin.flush()
            resp_line = _warmup_proc.stdout.readline()
        except (BrokenPipeError, OSError) as e:
            return {"error": f"warmup pipe: {e}", "exit_code": 1}
    if not resp_line:
        return {"error": "warmup closed stdout", "exit_code": 1}
    try:
        resp = json.loads(resp_line)
    except Exception as e:
        return {
            "error": f"bridge parse: {e}",
            "raw": resp_line.decode(errors="replace"),
            "exit_code": 1,
        }
    if "error" in resp:
        return {
            "error": resp["error"],
            "stack": resp.get("stack", ""),
            "exit_code": 1,
        }
    # Distinct field name from the Python eval path's `result` (which is
    # a Python repr() string). `result_json` is a JSON-encoded value; the
    # SDK json.loads it back into a native Python object. This keeps the
    # two eval paths cleanly distinguishable on the wire.
    return {"result_json": json.dumps(resp.get("result")), "exit_code": 0}


print(
    f"sandstorm: numpy={NUMPY_VERSION} agent starting in PID {os.getpid()} "
    f"({sys.executable})",
    flush=True,
)
_start_warmup()
print("sandstorm: parent VM ready for snapshot. children inherit this state.", flush=True)


class BufferedLineReader:
    """Persistent buffered line reader for socket connections.

    Unlike _recv_line which discards bytes after the first newline,
    this class preserves leftover bytes between calls so that coalesced
    TCP frames (multiple messages in one read) are handled correctly.
    """

    def __init__(self, conn: socket.socket):
        self._conn = conn
        self._buf = bytearray()

    def readline(self) -> bytes:
        """Read one complete line (terminated by \\n). Returns empty bytes on EOF."""
        while True:
            nl = self._buf.find(b"\n")
            if nl >= 0:
                line = bytes(self._buf[: nl + 1])
                del self._buf[: nl + 1]
                return line
            chunk = self._conn.recv(4096)
            if not chunk:
                # Return any remaining buffered data (partial line without newline)
                if self._buf:
                    remaining = bytes(self._buf)
                    self._buf.clear()
                    return remaining
                return b""
            self._buf.extend(chunk)

    def has_complete_line(self) -> bool:
        """Return True if the buffer already contains a complete line.

        Used by relay loops to consume buffered lines before selecting
        the socket — without this, a coalesced request+input frame is
        never delivered because the socket is not readable after the
        first recv() consumed both messages into the buffer.
        """
        return self._buf.find(b"\n") >= 0

    def try_readline(self) -> Optional[bytes]:
        """Return the next complete line from the buffer, or None if
        no complete line is currently buffered (no socket recv performed).

        Unlike readline(), this never blocks on the socket — it only
        returns a line if one is already present in the buffer.
        """
        nl = self._buf.find(b"\n")
        if nl < 0:
            return None
        line = bytes(self._buf[: nl + 1])
        del self._buf[: nl + 1]
        return line


def _recv_line(conn: socket.socket) -> bytes:
    buf = bytearray()
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            return bytes(buf)
        buf.extend(chunk)
        nl = buf.find(b"\n")
        if nl >= 0:
            return bytes(buf[: nl + 1])


def _send_json(conn: socket.socket, obj) -> None:
    conn.sendall((json.dumps(obj) + "\n").encode())


def handle(conn: socket.socket, addr) -> None:
    try:
        line_reader = BufferedLineReader(conn)
        line = line_reader.readline()
        if not line:
            return
        cmd = json.loads(line)
        action = cmd.get("action")

        if action == "ping":
            _send_json(
                conn,
                {
                    "pong": True,
                    "numpy_version": NUMPY_VERSION,
                    "pid": os.getpid(),
                    "agent_lang": AGENT_LANG,
                    "warmup_ready": _warmup_ready,
                    "path": _CONTAINER_PATH,
                },
            )

        elif action == "set_time":
            # The host sets a restored guest's clock, which is wherever it
            # was when the image was taken. In this process rather than by
            # exec'ing `date`, which was most of a warm start's 15 ms here.
            time.clock_settime(time.CLOCK_REALTIME, float(cmd["unix"]))
            _send_json(conn, {"ok": True})

        elif action == "exec":
            args = cmd["args"]
            timeout = cmd.get("timeout", 30)
            r = subprocess.run(
                args,
                capture_output=True,
                timeout=timeout,
                env=_subprocess_env(cmd.get("env")),
            )
            _send_json(
                conn,
                {
                    "stdout": r.stdout.decode("utf-8", "replace"),
                    "stderr": r.stderr.decode("utf-8", "replace"),
                    "exit_code": r.returncode,
                },
            )

        elif action == "eval":
            if AGENT_LANG == "node":
                _send_json(conn, _bridge_eval(cmd["code"]))
            else:
                try:
                    eval_globals = {}
                    np = _numpy()
                    if np is not None:
                        eval_globals["numpy"] = np
                        eval_globals["np"] = np
                    result = eval(cmd["code"], eval_globals)
                    _send_json(conn, {"result": repr(result), "exit_code": 0})
                except Exception as e:
                    _send_json(
                        conn,
                        {
                            "error": f"{type(e).__name__}: {e}",
                            "traceback": traceback.format_exc(),
                            "exit_code": 1,
                        },
                    )

        elif action == "stream":
            args = cmd["args"]
            cwd = cmd.get("cwd") or None
            env = _subprocess_env(cmd.get("env"))
            use_pty = cmd.get("pty", True)

            kwargs = {}
            if cwd:
                kwargs["cwd"] = cwd
            if env:
                kwargs["env"] = env

            if use_pty:
                master, slave = pty.openpty()
                try:
                    proc = subprocess.Popen(
                        args,
                        stdin=slave,
                        stdout=slave,
                        stderr=slave,
                        close_fds=True,
                        start_new_session=True,
                        **kwargs,
                    )
                except Exception:
                    os.close(master)
                    os.close(slave)
                    raise
                os.close(slave)
                out_fd = master
                in_fd = master
                err_fd = None
            else:
                proc = subprocess.Popen(
                    args,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    close_fds=True,
                    start_new_session=True,
                    **kwargs,
                )
                out_fd = proc.stdout.fileno()
                err_fd = proc.stderr.fileno()
                in_fd = proc.stdin.fileno()

            _send_json(conn, {"stream": "started", "pid": proc.pid, "pty": use_pty})

            # Event to signal that the reader thread has finished and sent
            # the exit code. The main loop uses select.select on the socket
            # with a timeout so it can check reader_done periodically.
            reader_done = threading.Event()

            # Reader thread: forward process output as JSON chunks.
            # In PTY mode, stdout and stderr are multiplexed on the master fd.
            # In non-PTY mode, stdout and stderr are separate pipes that must
            # both be drained — a child writing more than the pipe capacity
            # to stderr would block forever if stderr is never read.
            def _stream_reader():
                try:
                    if use_pty:
                        while True:
                            r, _, _ = select.select([out_fd], [], [], 0.5)
                            if not r:
                                if proc.poll() is not None:
                                    # Drain any remaining output.
                                    while True:
                                        try:
                                            data = os.read(out_fd, 65536)
                                            if not data:
                                                break
                                            _send_json(conn, {"out": data.decode("utf-8", "replace")})
                                        except OSError:
                                            break
                                    break
                                continue
                            try:
                                data = os.read(out_fd, 65536)
                            except OSError:
                                break
                            if not data:
                                break
                            _send_json(conn, {"out": data.decode("utf-8", "replace")})
                    else:
                        # Non-PTY: multiplex stdout and stderr via select.
                        # Track which fds are still open to avoid busy-looping
                        # on EOF'd pipes (which select reports as readable).
                        watched = [out_fd, err_fd]
                        eof_count = 0
                        while watched:
                            readable, _, _ = select.select(watched, [], [], 0.5)
                            if not readable:
                                if proc.poll() is not None and eof_count == len(watched):
                                    break
                                continue
                            for fd in readable:
                                try:
                                    data = os.read(fd, 65536)
                                except OSError:
                                    data = b""
                                if not data:
                                    # EOF on this stream — remove from watched
                                    # to prevent busy-looping.
                                    watched.remove(fd)
                                    eof_count += 1
                                    continue
                                label = "out" if fd == out_fd else "err"
                                _send_json(conn, {label: data.decode("utf-8", "replace")})
                        # All streams EOF'd — wait for process to exit.
                except (OSError, ValueError):
                    pass
                finally:
                    try:
                        _send_json(conn, {"exit_code": proc.wait()})
                    except Exception:
                        pass
                    reader_done.set()

            reader_thread = threading.Thread(target=_stream_reader, daemon=True)
            reader_thread.start()

            # Main loop: relay stdin lines from the client to the process.
            # Uses the same BufferedLineReader from the initial request so
            # coalesced TCP frames are handled correctly throughout.
            # Uses select.select on the socket with a timeout so reader_done
            # can interrupt the loop even when the client is idle.
            try:
                while not reader_done.is_set():
                    # Consume complete lines already buffered from a
                    # previous recv() before selecting the socket. If
                    # the initial request and first input arrived in
                    # one TCP read, the input is in the buffer but the
                    # socket is not readable — select would block and
                    # the buffered input would never be delivered.
                    if line_reader.has_complete_line():
                        line = line_reader.try_readline()
                    else:
                        # No buffered line — select the socket for the
                        # next recv, with a timeout so reader_done can
                        # interrupt the loop.
                        r, _, _ = select.select([conn], [], [], 0.5)
                        if not r:
                            continue  # No data; loop re-checks reader_done.
                        line = line_reader.readline()
                    if not line:
                        break
                    try:
                        msg = json.loads(line)
                    except json.JSONDecodeError:
                        continue  # Skip malformed input, don't kill session.
                    action_in = msg.get("action")
                    if action_in == "stop":
                        _kill_process_group(proc)
                        break
                    data = msg.get("in")
                    if data is not None and isinstance(data, str):
                        try:
                            if use_pty:
                                os.write(in_fd, data.encode("utf-8"))
                            else:
                                proc.stdin.write(data.encode("utf-8"))
                                proc.stdin.flush()
                        except (OSError, BrokenPipeError, ValueError):
                            break
            except Exception:
                pass
            finally:
                # Terminate the WHOLE process group (shell + descendants),
                # not just the direct child.
                _kill_process_group(proc)
                # Close stdin so processes waiting for EOF can exit gracefully.
                if not use_pty:
                    try:
                        proc.stdin.close()
                    except Exception:
                        pass
                # Wait for the reader thread to finish (with timeout) so the
                # exit_code message is sent before the socket closes.
                reader_thread.join(timeout=3)
                # Close the PTY master fd to prevent fd leaks.
                if use_pty:
                    try:
                        os.close(out_fd)
                    except OSError:
                        pass

        else:
            _send_json(conn, {"error": f"unknown action: {action}", "exit_code": 1})

    except Exception as e:
        try:
            _send_json(
                conn,
                {
                    "error": f"{type(e).__name__}: {e}",
                    "traceback": traceback.format_exc(),
                    "exit_code": 1,
                },
            )
        except OSError:
            pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def bind_address(cmdline: str = "/proc/cmdline") -> str:
    """The guest's own address, from the kernel's `ip=` argument.

    The host reaches the agent there, so nothing needs every interface. With
    no network there is no one to answer, and loopback is enough.
    """
    try:
        with open(cmdline) as f:
            words = f.read().split()
    except OSError:
        return "127.0.0.1"
    for word in words:
        if word.startswith("ip="):
            addr = word[len("ip="):].split(":")[0]
            try:
                socket.inet_aton(addr)
            except OSError:
                continue
            if addr.count(".") == 3:
                return addr
    return "127.0.0.1"


def serve() -> None:
    addr = bind_address()
    # Retry bind — eth0 might not be fully up at startup.
    last_err = None
    for _ in range(30):
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind((addr, 8888))
            s.listen(128)
            break
        except OSError as e:
            last_err = e
            time.sleep(0.2)
    else:
        print(f"sandstorm: failed to bind {addr}:8888 after retries: {last_err}", flush=True)
        sys.exit(1)

    print(f"sandstorm: agent listening on {addr}:8888", flush=True)

    while True:
        try:
            conn, peer = s.accept()
            threading.Thread(target=handle, args=(conn, peer), daemon=True).start()
        except Exception as e:
            print(f"sandstorm: accept error: {e}", flush=True)
            time.sleep(0.1)


if __name__ == "__main__":
    serve()
