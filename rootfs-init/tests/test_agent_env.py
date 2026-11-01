#!/usr/bin/env python3
"""Unit tests for sandstorm-agent.py container environment helpers.

Tests _load_container_env (parsing /etc/environment) and _subprocess_env
(merging caller env on top of GUEST_ENV). These are pure-Python functions
that don't require firecracker or a real rootfs.

Run with: python3 -m pytest rootfs-init/tests/test_agent_env.py -v
Or:      python3 rootfs-init/tests/test_agent_env.py
"""
import importlib
import os
import sys
import tempfile
import unittest


def _load_agent_module():
    """Import sandstorm-agent.py as a module, handling its module-level side effects."""
    # Save and restore os.environ to avoid test pollution
    orig_env = dict(os.environ)
    try:
        # The agent reads /etc/environment at import time, so we point it at
        # a temp file we control.
        with tempfile.NamedTemporaryFile(mode='w', suffix='.env', delete=False) as f:
            f.write('PATH=/usr/local/test:/usr/bin\n')
            f.write('CARGO_HOME=/usr/local/cargo\n')
            f.write('# comment line\n')
            f.write('EMPTY=\n')
            env_path = f.name

        # Patch the default path before import
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            'sandstorm_agent_test', os.path.join(os.path.dirname(__file__), '..', 'sandstorm-agent.py')
        )
        module = importlib.util.module_from_spec(spec)

        # Monkey-p the default path argument
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


class TestLoadContainerEnv(unittest.TestCase):
    """Test _load_container_env parsing of /etc/environment format."""

    def setUp(self):
        self.module, self.env_path = _load_agent_module()

    def tearDown(self):
        os.unlink(self.env_path)

    def test_parses_simple_key_value(self):
        """Simple KEY=VALUE pairs are parsed correctly."""
        result = self.module._load_container_env(self.env_path)
        self.assertEqual(result.get('CARGO_HOME'), '/usr/local/cargo')

    def test_parses_path(self):
        """PATH is parsed from /etc/environment."""
        result = self.module._load_container_env(self.env_path)
        self.assertEqual(result.get('PATH'), '/usr/local/test:/usr/bin')

    def test_skips_comments(self):
        """Comment lines starting with # are skipped."""
        result = self.module._load_container_env(self.env_path)
        self.assertNotIn('# comment', result)

    def test_empty_value(self):
        """Empty values are preserved as empty strings."""
        result = self.module._load_container_env(self.env_path)
        self.assertEqual(result.get('EMPTY'), '')

    def test_missing_file_returns_empty(self):
        """Missing file returns empty dict."""
        result = self.module._load_container_env('/nonexistent/path/file')
        self.assertEqual(result, {})

    def test_quoted_values(self):
        """Quoted values have their quotes stripped."""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.env', delete=False) as f:
            f.write('VAR1="double quoted"\n')
            f.write("VAR2='single quoted'\n")
            f.write('VAR3=unquoted\n')
            qpath = f.name
        try:
            result = self.module._load_container_env(qpath)
            self.assertEqual(result['VAR1'], 'double quoted')
            self.assertEqual(result['VAR2'], 'single quoted')
            self.assertEqual(result['VAR3'], 'unquoted')
        finally:
            os.unlink(qpath)

    def test_equals_in_value(self):
        """Values containing = are preserved (first = is the separator)."""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.env', delete=False) as f:
            f.write('URL=http://host:8080/path?a=b\n')
            eqpath = f.name
        try:
            result = self.module._load_container_env(eqpath)
            self.assertEqual(result['URL'], 'http://host:8080/path?a=b')
        finally:
            os.unlink(eqpath)

    def test_lines_without_equals_skipped(self):
        """Lines without = are silently skipped."""
        with tempfile.NamedTemporaryFile(mode='w', suffix='.env', delete=False) as f:
            f.write('no_equals_here\n')
            f.write('GOOD=value\n')
            nepath = f.name
        try:
            result = self.module._load_container_env(nepath)
            self.assertEqual(result, {'GOOD': 'value'})
        finally:
            os.unlink(nepath)


class TestSubprocessEnv(unittest.TestCase):
    """Test _subprocess_env merge semantics."""

    def setUp(self):
        self.module, self.env_path = _load_agent_module()

    def tearDown(self):
        os.unlink(self.env_path)

    def test_none_returns_copy_of_guest_env(self):
        """None caller_env returns a copy of GUEST_ENV."""
        result = self.module._subprocess_env(None)
        self.assertIn('PATH', result)
        self.assertEqual(result['PATH'], '/usr/local/test:/usr/bin')
        # Verify it's a copy, not the original
        result['TEST_KEY'] = 'test'
        self.assertNotIn('TEST_KEY', self.module.GUEST_ENV)

    def test_caller_env_merged(self):
        """Caller env is merged on top of GUEST_ENV."""
        caller = {'CUSTOM': 'value'}
        result = self.module._subprocess_env(caller)
        self.assertEqual(result['CUSTOM'], 'value')
        self.assertIn('PATH', result)

    def test_caller_path_overrides(self):
        """Caller can override PATH."""
        caller = {'PATH': '/override:/bin'}
        result = self.module._subprocess_env(caller)
        self.assertEqual(result['PATH'], '/override:/bin')

    def test_guest_env_vars_preserved(self):
        """Non-PATH GUEST_ENV vars are preserved when caller provides env."""
        caller = {'EXTRA': 'val'}
        result = self.module._subprocess_env(caller)
        self.assertIn('CARGO_HOME', result)



class FakeSocket:
    """Fake socket that returns pre-loaded data sequentially."""
    def __init__(self, *chunks):
        self._data = b"".join(chunks)
        self._pos = 0

    def recv(self, size):
        if self._pos >= len(self._data):
            return b""
        chunk = self._data[self._pos:self._pos + size]
        self._pos += len(chunk)
        return chunk


class TestBufferedLineReader(unittest.TestCase):
    """Test BufferedLineReader for coalesced TCP frame handling."""

    def setUp(self):
        self.module, self.env_path = _load_agent_module()
        self.BLR = self.module.BufferedLineReader

    def tearDown(self):
        os.unlink(self.env_path)

    def test_single_line(self):
        sock = FakeSocket(b'{"action":"ping"}\n')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'{"action":"ping"}\n')

    def test_coalesced_frames(self):
        """Two messages in one TCP read are both returned correctly."""
        sock = FakeSocket(b'msg1\nmsg2\n')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'msg1\n')
        self.assertEqual(reader.readline(), b'msg2\n')

    def test_partial_line_across_reads(self):
        """A line split across multiple recv calls is assembled correctly."""
        sock = FakeSocket(b'par', b'tial', b'\n')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'partial\n')

    def test_eof_returns_empty(self):
        sock = FakeSocket(b'')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'')

    def test_eof_with_partial_line(self):
        """EOF with partial line (no newline) returns the remaining bytes."""
        sock = FakeSocket(b'partial')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'partial')

    def test_multiple_lines_one_read(self):
        """Multiple lines in a single recv are returned one at a time."""
        sock = FakeSocket(b'line1\nline2\nline3\n')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'line1\n')
        self.assertEqual(reader.readline(), b'line2\n')
        self.assertEqual(reader.readline(), b'line3\n')

    def test_interleaved_reads(self):
        """Coalesced frames split across recv boundaries work correctly."""
        sock = FakeSocket(b'msg1\nmsg', b'2\nmsg3\n')
        reader = self.BLR(sock)
        self.assertEqual(reader.readline(), b'msg1\n')
        self.assertEqual(reader.readline(), b'msg2\n')
        self.assertEqual(reader.readline(), b'msg3\n')
class TestBindAddress(unittest.TestCase):
    """The agent listens on the guest's address, not on every interface."""

    def setUp(self):
        self.agent, env_path = _load_agent_module()
        self.addCleanup(os.unlink, env_path)

    def bound(self, cmdline):
        with tempfile.NamedTemporaryFile(mode='w', delete=False) as f:
            f.write(cmdline)
        self.addCleanup(os.unlink, f.name)
        return self.agent.bind_address(f.name)

    def test_the_address_the_host_boots_a_guest_with(self):
        got = self.bound(
            "quiet panic=1 ip=10.42.0.2::10.42.0.1:255.255.255.252::eth0:off sandstorm.app=00\n"
        )
        self.assertEqual(got, "10.42.0.2")

    def test_no_network_stays_on_loopback(self):
        self.assertEqual(self.bound("quiet panic=1\n"), "127.0.0.1")

    def test_an_ip_argument_with_no_address_stays_on_loopback(self):
        self.assertEqual(self.bound("ip=dhcp\n"), "127.0.0.1")
        self.assertEqual(self.bound("ip=off\n"), "127.0.0.1")

    def test_an_unreadable_cmdline_stays_on_loopback(self):
        self.assertEqual(self.agent.bind_address("/no/such/file"), "127.0.0.1")


if __name__ == '__main__':
    unittest.main(verbosity=2)
