#!/usr/bin/env python3
"""Run fleet.sh against a loopback master without SSHing to any host."""

import http.server
import json
import os
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path


FLEET = Path(__file__).resolve().parents[1] / "fleet.sh"
PROBE = "/v1/sandboxes/test-sandbox"
SSH = """#!/usr/bin/env python3
import os
import shlex
import subprocess
import sys
from urllib.parse import urlsplit

host, command = sys.argv[-2:]
if (host, command) == ("test-node", "sudo bash -s"):
    # Accept the rootfs install script without executing it.
    sys.stdin.read()
    sys.exit(0)
if host != "test-master":
    sys.exit(f"unexpected SSH target: {host}")

args = shlex.split(command)
if args[:2] != ["sudo", "curl"]:
    sys.exit("unexpected SSH command")
args = args[2:]
url = urlsplit(args.pop())
if url.scheme != "https" or url.hostname != "127.0.0.1" or url.port != int(os.environ["FLEET_TEST_PORT"]):
    sys.exit("unexpected master URL")

# Adapt only TLS and authentication for the fixture. Let real curl process
# the production status-handling flags, headers and body.
cert = args.index("--cacert")
del args[cert:cert + 2]
auth = args.index("authorization: Bearer $(sudo cat /var/lib/sandstorm/pki/api-token)")
args[auth] = "authorization: Bearer test-token"
url = url._replace(scheme="http").geturl()
sys.exit(subprocess.run(["curl", "--disable", *args, url]).returncode)
"""


class FleetHttpTest(unittest.TestCase):
    def run_fleet(self, command, responses):
        observed = []

        class Master(http.server.BaseHTTPRequestHandler):
            def respond(self):
                request = (self.command, self.path)
                length = int(self.headers.get("Content-Length", 0))
                self.rfile.read(length)
                observed.append(request)
                status, body = responses.get(request, (500, {"error": "unexpected request"}))
                self.send_response(status)
                if body is not None:
                    self.send_header("Content-Type", "application/json")
                self.end_headers()
                if body is not None:
                    self.wfile.write(json.dumps(body).encode())

            do_POST = do_GET = do_DELETE = respond

            def log_message(self, *_args):
                pass

        with tempfile.TemporaryDirectory() as tmp, http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), Master
        ) as master:
            directory = Path(tmp)
            ssh = directory / "ssh"
            ssh.write_text(SSH)
            ssh.chmod(0o755)
            sleep = directory / "sleep"
            sleep.write_text("#!/bin/sh\nexit 0\n")
            sleep.chmod(0o755)
            (directory / "fleet.env").write_text(
                "MASTER=test-master\n"
                "MASTER_ADDR=test-master\n"
                "BIND_ADDR=127.0.0.1\n"
                f"PORT={master.server_port}\n"
                "NODES=test-node\n"
                "POOL=default\n"
                "CA=test-ca.pem\n"
            )
            thread = threading.Thread(
                target=master.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
            )
            thread.start()
            try:
                result = subprocess.run(
                    ["bash", str(FLEET), command],
                    env={
                        **os.environ,
                        "PATH": f"{directory}{os.pathsep}{os.environ['PATH']}",
                        "no_proxy": "127.0.0.1",
                        "NO_PROXY": "127.0.0.1",
                        "SANDSTORM_FLEET_STATE": tmp,
                        "FLEET_TEST_PORT": str(master.server_port),
                    },
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=10,
                )
            finally:
                master.shutdown()
                thread.join()

        self.assertEqual(observed, list(responses))
        return result

    def test_seed_reports_template_registration_conflict(self):
        result = self.run_fleet(
            "seed", {("POST", "/v1/templates"): (409, {"error": "template already exists"})}
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("the master refused the template", result.stdout)
        self.assertIn("HTTP 409", result.stdout)
        self.assertIn("template already exists", result.stdout)
        self.assertNotIn("template 'py' registered", result.stdout)

    def test_seed_reports_successful_registration(self):
        result = self.run_fleet("seed", {("POST", "/v1/templates"): (201, None)})
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("template 'py' registered", result.stdout)

    def test_verify_reports_probe_cleanup_failure(self):
        result = self.run_verify(500, {"error": "probe deletion unavailable"})
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("could not delete probe sandbox test-sandbox", result.stdout)
        self.assertIn("HTTP 500", result.stdout)
        self.assertIn("probe deletion unavailable", result.stdout)

    def test_verify_cleans_up_after_probe_status_failure(self):
        result = self.run_verify(
            204, None, status_reply=(500, {"error": "probe status unavailable"})
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("could not check probe sandbox test-sandbox", result.stdout)
        self.assertIn("HTTP 500", result.stdout)
        self.assertIn("probe status unavailable", result.stdout)

    def test_verify_reports_probe_creation_refusal(self):
        result = self.run_fleet(
            "verify", {("POST", "/v1/sandboxes"): (409, {"error": "node cannot admit work"})}
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("the master would not create a probe sandbox", result.stdout)
        self.assertIn("HTTP 409", result.stdout)
        self.assertIn("node cannot admit work", result.stdout)

    def test_verify_reports_success_after_probe_cleanup(self):
        result = self.run_verify(204, None)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("the fleet can actually do work", result.stdout)

    def run_verify(self, status, body, *, status_reply=(200, {"ip": "192.0.2.2"})):
        return self.run_fleet(
            "verify",
            {
                ("POST", "/v1/sandboxes"): (201, {"id": "test-sandbox"}),
                ("GET", PROBE): status_reply,
                ("DELETE", PROBE): (status, body),
            },
        )


if __name__ == "__main__":
    unittest.main()
