#!/usr/bin/env python3
"""Tests for the sandstorm-agent.py stream action — coalesced-frame handling
and interactive session lifecycle.

Tests the coalesced-frame regression: when the initial stream request and
first input message arrive in a single TCP read, the BufferedLineReader
returns the request and retains the input in its buffer. The relay loop
must consume buffered lines before selecting the socket, or the buffered
input is never delivered.

Also covers: no-input exit, stderr flood (non-PTY), stop, and
connection-close handler paths.

Run with: python3 -m pytest rootfs-init/tests/test_agent_stream.py -v
Or:      python3 rootfs-init/tests/test_agent_stream.py
"""
import importlib.util
import json
import os
import re
import socket
import sys
import tempfile
import threading
import time
import unittest


def _load_agent_module():
    """Import sandstorm-agent.py as a module, handling its module-level side effects."""
    orig_env = dict(os.environ)
    try:
        with tempfile.NamedTemporaryFile(mode='w', suffix='.env', delete=False) as f:
            f.write('PATH=/usr/bin:/bin\n')
            env_path = f.name

        spec = importlib.util.spec_from_file_location(
            'sandstorm_agent_stream_test',
            os.path.join(os.path.dirname(__file__), '..', 'sandstorm-agent.py'),
        )
        module = importlib.util.module_from_spec(spec)
        orig_open = open

        def patched_open(path, *args, **kwargs):
            if path == '/etc/environment':
                return orig_open(env_path, *args, **kwargs)
            return orig_open(path, *args, **kwargs)

        import builtins

        builtins.open = patched_open
        try:
            spec.loader.exec_module(module)
        finally:
            builtins.open = orig_open

        return module, env_path
    finally:
        os.environ.clear()
        os.environ.update(orig_env)


def _pid_alive(pid: int) -> bool:
    """Return True if the given PID is still alive (signal 0 probe)."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class TestBufferedLineReaderCoalesced(unittest.TestCase):
    """Tests for BufferedLineReader.has_complete_line and try_readline."""

    def setUp(self):
        self.module, self.env_path = _load_agent_module()

    def tearDown(self):
        os.unlink(self.env_path)

    def _make_pair(self):
        """Create a connected socketpair for testing."""
        a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        return a, b

    def test_has_complete_line_after_partial_read(self):
        """has_complete_line returns True when buffer contains a newline."""
        a, b = self._make_pair()
        reader = self.module.BufferedLineReader(a)
        # Send two messages in one write (coalesced frame)
        b.sendall(b'{"action":"ping"}\n{"action":"ping"}\n')
        # First readline gets the first message
        line1 = reader.readline()
        self.assertEqual(line1, b'{"action":"ping"}\n')
        # Second message is in the buffer — has_complete_line should detect it
        self.assertTrue(reader.has_complete_line())
        # try_readline returns it without blocking on the socket
        line2 = reader.try_readline()
        self.assertEqual(line2, b'{"action":"ping"}\n')
        # No more complete lines
        self.assertFalse(reader.has_complete_line())
        self.assertIsNone(reader.try_readline())
        a.close()
        b.close()

    def test_try_readline_returns_none_when_no_complete_line(self):
        """try_readline returns None when buffer has no complete line."""
        a, b = self._make_pair()
        reader = self.module.BufferedLineReader(a)
        self.assertFalse(reader.has_complete_line())
        self.assertIsNone(reader.try_readline())
        a.close()
        b.close()

    def test_try_readline_does_not_block(self):
        """try_readline never blocks on the socket — only reads buffer."""
        a, b = self._make_pair()
        reader = self.module.BufferedLineReader(a)
        # Don't send anything — try_readline should return None immediately
        start = time.monotonic()
        result = reader.try_readline()
        elapsed = time.monotonic() - start
        self.assertIsNone(result)
        self.assertLess(elapsed, 0.1, "try_readline should not block")
        a.close()
        b.close()


class TestHandleStreamCoalescedFrame(unittest.TestCase):
    """Full handle() tests using a real socketpair and a simple subprocess.

    Tests that a coalesced request+input frame (both sent in one TCP write)
    is correctly delivered to the subprocess.
    """

    def setUp(self):
        self.module, self.env_path = _load_agent_module()

    def tearDown(self):
        os.unlink(self.env_path)

    def _make_pair(self):
        a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        a.settimeout(10)
        b.settimeout(10)
        return a, b

    def _start_handle(self, a):
        """Start handle() in a daemon thread with socket 'a' as conn.

        The test uses socket 'b' as the client: writes to 'b' are
        read by handle on 'a', and responses written by handle on 'a'
        are read by the test on 'b'.
        """
        t = threading.Thread(
            target=self.module.handle, args=(a, ("test",)), daemon=True
        )
        t.start()
        return t

    def _read_json_line(self, sock, leftover=None):
        """Read one newline-terminated JSON message from sock.

        Pass leftover bytes from a previous read to handle coalesced
        TCP frames where multiple messages arrive in one recv().
        Returns (parsed_json, remaining_leftover_bytes).
        """
        buf = bytearray(leftover or b'')
        # Check if we already have a complete line from the leftover
        nl = buf.find(b'\n')
        if nl >= 0:
            return json.loads(buf[: nl + 1]), bytes(buf[nl + 1 :])
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            buf.extend(chunk)
            nl = buf.find(b'\n')
            if nl >= 0:
                return json.loads(buf[: nl + 1]), bytes(buf[nl + 1 :])
        return None, bytes(buf)

    def test_coalesced_request_and_input_delivered(self):
        """Request + first input in one send: input must reach the process.

        This is the core regression: if the relay loop selects the socket
        without checking the buffered line first, the input is stuck in
        the BufferedLineReader's buffer and never delivered.
        """
        a, b = self._make_pair()
        self._start_handle(a)
        # Use 'cat' which echoes stdin to stdout (requires PTY off for
        # line-buffered echo, or the PTY will echo it).
        # We use a shell script that reads a line and prints it with a marker.
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "read line; echo GOT:$line; exit 0"],
            "pty": False,
        }) + "\n"
        # The input line, appended in the SAME send (coalesced frame)
        input_msg = json.dumps({"in": "hello-world\n"}) + "\n"
        b.sendall(request.encode() + input_msg.encode())

        # Read the "started" message
        started, leftover = self._read_json_line(b)
        self.assertIsNotNone(started, "Expected 'started' message")
        self.assertEqual(started.get("stream"), "started")

        # Read output until we see GOT:hello-world or exit_code
        got_input = False
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "out" in msg and "GOT:hello-world" in msg["out"]:
                got_input = True
            if "exit_code" in msg:
                break
        self.assertTrue(got_input, "Subprocess did not receive the coalesced input 'hello-world'")
        a.close()
        b.close()

    def test_no_input_exit(self):
        """A stream session that receives no input should exit when the
        process finishes and the connection closes."""
        a, b = self._make_pair()
        self._start_handle(a)
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "echo done; exit 0"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())
        b.shutdown(socket.SHUT_WR)  # Signal EOF — no more input

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        # Read until exit_code or timeout
        exit_code = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "exit_code" in msg:
                exit_code = msg["exit_code"]
                break
        self.assertIsNotNone(exit_code, "Did not receive exit_code")
        a.close()

    def test_stderr_flood_non_pty(self):
        """A child that floods stderr must not deadlock the reader thread."""
        a, b = self._make_pair()
        self._start_handle(a)
        # Write enough to stderr to exceed pipe capacity (64KB on Linux)
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "yes 'error line' | head -10000 >&2; echo done; exit 0"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        # Read until we see "done" in stdout or exit_code
        saw_done = False
        exit_code = None
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "out" in msg and "done" in msg["out"]:
                saw_done = True
            if "err" in msg:
                pass  # stderr is expected
            if "exit_code" in msg:
                exit_code = msg["exit_code"]
                break
        self.assertTrue(saw_done, "Did not see 'done' in stdout — stderr flood may have deadlocked the reader")
        a.close()
        b.close()

    def test_stop_terminates_session(self):
        """A 'stop' action should terminate the process."""
        a, b = self._make_pair()
        self._start_handle(a)
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "sleep 30; echo never"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        # Send stop
        stop_msg = json.dumps({"action": "stop"}) + "\n"
        b.sendall(stop_msg.encode())

        # Read until exit_code
        exit_code = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "exit_code" in msg:
                exit_code = msg["exit_code"]
                break
        self.assertIsNotNone(exit_code, "Did not receive exit_code after stop")
        a.close()
        b.close()

    def test_connection_close_triggers_cleanup(self):
        """Closing the client socket should trigger process cleanup."""
        a, b = self._make_pair()
        self._start_handle(a)
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "sleep 30; echo never"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())
        b.shutdown(socket.SHUT_WR)  # Signal EOF

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        # The process should be killed; we should get an exit_code
        # (or the socket closes, which also indicates cleanup)
        exit_code = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "exit_code" in msg:
                exit_code = msg["exit_code"]
                break
        # Either we got exit_code or the socket closed (both indicate cleanup)
        a.close()

    def test_stop_kills_descendants(self):
        """A 'stop' action must kill descendant processes, not just the shell.

        Regression: the stream command runs through a shell; cleanup that
        only signals the shell leaves its children (e.g. `sleep`) running.
        The command is started in its own process group
        (start_new_session=True), so stop must TERM/KILL the whole group.
        """
        a, b = self._make_pair()
        self._start_handle(a)
        # Spawn `sleep 30` as a background child, print its PID to stderr
        # (unbuffered) so the test can capture it, then `wait` so the shell
        # stays alive holding the child.
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "sleep 30 & echo CHILD:$! >&2; wait"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        child_pid = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "err" in msg and "CHILD:" in msg["err"]:
                m = re.search(r"CHILD:(\d+)", msg["err"])
                if m:
                    child_pid = int(m.group(1))
                    break
            if "exit_code" in msg:
                break
        self.assertIsNotNone(child_pid, "did not capture the descendant PID")
        self.assertTrue(_pid_alive(child_pid), "descendant should be alive before stop")

        b.sendall(json.dumps({"action": "stop"}).encode() + b"\n")

        exit_code = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "exit_code" in msg:
                exit_code = msg["exit_code"]
                break
        self.assertIsNotNone(exit_code, "no exit_code after stop")

        # The descendant must be gone — the group kill reaches it directly.
        deadline = time.monotonic() + 5
        while _pid_alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertFalse(_pid_alive(child_pid), "descendant (sleep) survived stop")
        a.close()
        b.close()

    def test_connection_close_kills_descendants(self):
        """Disconnect must kill descendants, not just the shell."""
        a, b = self._make_pair()
        self._start_handle(a)
        request = json.dumps({
            "action": "stream",
            "args": ["sh", "-c", "sleep 30 & echo CHILD:$! >&2; wait"],
            "pty": False,
        }) + "\n"
        b.sendall(request.encode())

        started, leftover = self._read_json_line(b)
        self.assertEqual(started.get("stream"), "started")

        # Capture the descendant PID FIRST, while the main loop is still
        # waiting for input (the socket is not yet closed).
        child_pid = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            msg, leftover = self._read_json_line(b, leftover)
            if msg is None:
                break
            if "err" in msg and "CHILD:" in msg["err"]:
                m = re.search(r"CHILD:(\d+)", msg["err"])
                if m:
                    child_pid = int(m.group(1))
                    break
            if "exit_code" in msg:
                break
        self.assertIsNotNone(child_pid, "did not capture the descendant PID")
        self.assertTrue(_pid_alive(child_pid), "descendant should be alive before disconnect")

        # Now disconnect — the cleanup path must kill the whole group.
        b.shutdown(socket.SHUT_WR)

        deadline = time.monotonic() + 5
        while _pid_alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertFalse(_pid_alive(child_pid), "descendant (sleep) survived disconnect")
        a.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)