#!/usr/bin/env python3
"""Unit tests for how the agent reads the template's program off the kernel
command line (the Firecracker backend's `sandstorm.app=`).

Run with: python3 rootfs-init/tests/test_agent_app.py
"""
import importlib.util
import json
import os
import unittest


def _load_agent():
    spec = importlib.util.spec_from_file_location(
        "sandstorm_agent_app_test",
        os.path.join(os.path.dirname(__file__), "..", "sandstorm-agent.py"),
    )
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


AGENT = _load_agent()


def _hex(obj) -> str:
    return json.dumps(obj).encode().hex()


class AppFromCmdline(unittest.TestCase):
    def test_reads_the_program(self):
        app = {"args": ["python3", "-m", "http.server", "8000"], "env": ["A=1"], "cwd": "/app"}
        line = f"console=ttyS0 init=/sandstorm-init.sh sandstorm.app={_hex(app)} quiet"
        self.assertEqual(AGENT._app_from_cmdline(line), app)

    def test_none_without_one(self):
        self.assertIsNone(AGENT._app_from_cmdline("console=ttyS0 init=/sandstorm-init.sh"))

    def test_unreadable_or_empty_is_ignored(self):
        self.assertIsNone(AGENT._app_from_cmdline("sandstorm.app=zz"))
        self.assertIsNone(AGENT._app_from_cmdline(f"sandstorm.app={_hex({'args': []})}"))


if __name__ == "__main__":
    unittest.main()
