"""Sandbox against a stand-in master.

No daemon or CLI needed: `FakeMaster` is a small HTTP server in this process
that answers the way `crates/sandstorm-core/src/master/api.rs` does, including
creating a sandbox before any node has started it, and refusing exec, eval
and ping without that sandbox's token.
"""
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import sandstorm
from sandstorm import Controller, ControllerError, Sandbox
from sandstorm.controller import SANDBOX_TOKEN_HEADER

API_TOKEN = "api-secret"


class FakeMaster:
    """One sandbox id per create; `reports_until_running` polls to come up."""

    def __init__(self):
        self.requests = []
        self.sandboxes = {}
        self.reports_until_running = 0
        self.last_error = None
        self.error_attempts = 0
        self.exec_delay = 0.0
        self.eval_reply = {"result": "4\n", "exit_code": 0}
        master = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _reply(self, status, body=None):
                raw = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def _handle(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length)) if length else None
                master.requests.append((self.command, self.path, body, self.headers))
                if self.headers.get("Authorization") != f"Bearer {API_TOKEN}":
                    return self._reply(401, {"error": "bad bearer token"})
                self._reply(*master.answer(self.command, self.path, body, self.headers))

            do_GET = do_POST = do_DELETE = _handle

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(
            target=self.server.serve_forever, args=(0.01,), daemon=True
        ).start()

    def answer(self, method, path, body, headers):
        parts = path.split("/")[2:]
        if method == "POST" and parts == ["sandboxes"]:
            return self.create(body)
        if method == "GET" and parts == ["templates"]:
            return 200, ["python"]
        sandbox_id = parts[1]
        sb = self.sandboxes.get(sandbox_id)
        if sb is None:
            return 404, {"error": "no such sandbox"}
        if method == "GET":
            return 200, self.view(sb)
        if method == "DELETE":
            del self.sandboxes[sandbox_id]
            return 204, None
        if parts[2] == "touch":
            return 200, self.view(sb)
        if parts[2] == "branch":
            return 201, {"tag": body["tag"], "from_sandbox": sandbox_id}
        if headers.get(SANDBOX_TOKEN_HEADER) != sb["token"]:
            return 403, {"error": "this sandbox requires its own access token"}
        if parts[2] == "exec":
            time.sleep(self.exec_delay)
            return 200, {"stdout": "ok\n", "stderr": "", "exit_code": 0}
        if parts[2] == "eval":
            return 200, self.eval_reply
        return 200, {"ok": True, "tier": "hot", "guest_addr": "10.86.0.2:8080", "latency_ms": 1}

    def create(self, body):
        if ("template" in body) == ("snapshot_tag" in body):
            return 400, {"error": "one of template or snapshot_tag is required"}
        n = len(self.sandboxes) + 1
        sb = {"id": f"sb-{n}", "token": f"token-{n}", "reports": 0, "tier": "hot"}
        self.sandboxes[sb["id"]] = sb
        # As the master does: the token only here, and no address yet.
        return 201, {"id": sb["id"], "token": sb["token"], "tier": "hot"}

    def view(self, sb):
        sb["reports"] += 1
        seen = {"id": sb["id"], "tier": sb["tier"], "error_attempts": self.error_attempts}
        if self.last_error:
            seen["last_error"] = self.last_error
        if self.reports_until_running is not None and sb["reports"] > self.reports_until_running:
            seen["ip"] = "10.86.0.2"
            seen["guest_addr"] = "10.86.0.2:8080"
        return seen

    def seen(self, method, suffix=""):
        return [r for r in self.requests if r[0] == method and r[1].endswith(suffix)]


@pytest.fixture
def master():
    m = FakeMaster()
    yield m
    m.server.shutdown()
    m.server.server_close()


@pytest.fixture
def controller(master):
    return Controller(base_url=master.url, token=API_TOKEN)


@pytest.fixture(autouse=True)
def no_defaults(monkeypatch):
    monkeypatch.delenv("SANDSTORM_TEMPLATE", raising=False)
    monkeypatch.delenv("SANDSTORM_TAG", raising=False)


# ---- the package ------------------------------------------------------------


def test_the_default_address_is_where_a_master_listens(monkeypatch):
    monkeypatch.delenv("SANDSTORM_URL", raising=False)
    monkeypatch.delenv("SANDSTORM_CA", raising=False)
    assert Controller().base_url == "https://127.0.0.1:8080"


def test_the_package_reports_the_version_it_is_published_as():
    pyproject = Path(__file__).parent.parent / "pyproject.toml"
    version = re.search(r'^version = "(.+)"$', pyproject.read_text(), re.MULTILINE)
    assert sandstorm.__version__ == version.group(1)


# ---- creating ---------------------------------------------------------------


def test_creates_from_a_template_with_no_cli_on_path(master, controller, monkeypatch):
    monkeypatch.setenv("PATH", "")
    sb = Sandbox("py", controller=controller)
    assert (sb.id, sb.sandbox_id, sb.token) == ("sb-1", "sb-1", "token-1")
    _, _, body, headers = master.seen("POST", "/v1/sandboxes")[0]
    # No `n`: that selects the v0.5 list response.
    assert body == {"template": "py"}
    assert headers.get(SANDBOX_TOKEN_HEADER) is None


def test_creates_from_a_tag(master, controller):
    Sandbox(tag="checkpoint-1", memory_limit_mib=256, controller=controller)
    assert master.seen("POST", "/v1/sandboxes")[0][2] == {
        "snapshot_tag": "checkpoint-1",
        "memory_limit_mib": 256,
    }


def test_create_is_an_alias(controller):
    assert Sandbox.create("py", controller=controller).id == "sb-1"


@pytest.mark.parametrize(
    ("variable", "field"),
    [("SANDSTORM_TEMPLATE", "template"), ("SANDSTORM_TAG", "snapshot_tag")],
)
def test_defaults_come_from_the_environment(master, controller, monkeypatch, variable, field):
    monkeypatch.setenv(variable, "from-env")
    Sandbox(controller=controller)
    assert master.seen("POST", "/v1/sandboxes")[0][2] == {field: "from-env"}


def test_an_explicit_source_beats_the_environment(master, controller, monkeypatch):
    monkeypatch.setenv("SANDSTORM_TEMPLATE", "from-env")
    Sandbox(tag="explicit", controller=controller)
    assert master.seen("POST", "/v1/sandboxes")[0][2] == {"snapshot_tag": "explicit"}


def test_needs_a_template_or_a_tag(master, controller):
    with pytest.raises(ValueError, match="template or a tag"):
        Sandbox(controller=controller)
    with pytest.raises(ValueError, match="template or a tag"):
        Sandbox("py", tag="t", controller=controller)
    assert master.requests == []


def test_a_refused_create_raises_with_the_masters_status(master, controller):
    master.create = lambda body: (404, {"error": "no snapshot gone"})
    with pytest.raises(ControllerError) as e:
        Sandbox(tag="gone", controller=controller)
    assert e.value.status == 404


# ---- waiting for it to start ------------------------------------------------


def test_waits_for_a_node_to_report_it_running(master, controller):
    master.reports_until_running = 2
    sb = Sandbox("py", controller=controller)
    assert len(master.seen("GET", "/v1/sandboxes/sb-1")) == 3
    # What it learned is merged in, and the token survives the merge.
    assert sb.info["guest_addr"] == "10.86.0.2:8080"
    assert sb.info["token"] == "token-1"


def test_a_start_the_master_gave_up_on_is_reported_and_removed(master, controller):
    master.reports_until_running = None
    master.error_attempts = 5
    master.last_error = "runsc: no such template"
    with pytest.raises(RuntimeError, match="failed to start: runsc: no such template"):
        Sandbox("py", start_timeout=30, controller=controller)
    assert master.sandboxes == {}


def test_a_start_that_never_finishes_times_out_and_is_removed(master, controller):
    master.reports_until_running = None
    master.last_error = "still pulling"
    with pytest.raises(RuntimeError, match="did not start.*still pulling"):
        Sandbox("py", start_timeout=0.2, controller=controller)
    assert master.sandboxes == {}


def test_a_failed_cleanup_keeps_the_start_error(master, controller, monkeypatch):
    master.reports_until_running = None

    def refuse(_id):
        raise ControllerError(500, {"error": "down"}, "url")

    monkeypatch.setattr(controller, "kill_sandbox", refuse)
    with pytest.raises(RuntimeError, match="did not start"):
        Sandbox("py", start_timeout=0.1, controller=controller)


# ---- the sandbox's token ----------------------------------------------------


def test_exec_eval_and_ping_carry_the_token(master, controller):
    sb = Sandbox("py", controller=controller)
    assert sb.commands.run("true").exit_code == 0
    sb.eval("print(2 + 2)")
    assert sb.ping()["ok"] is True
    for suffix in ("/exec", "/eval", "/ping"):
        assert master.seen("POST", suffix)[0][3].get(SANDBOX_TOKEN_HEADER) == "token-1"


def test_two_sandboxes_keep_their_own_tokens(master, controller):
    a = Sandbox("py", controller=controller)
    b = Sandbox("py", controller=controller)
    a.commands.run("true")
    b.commands.run("true")
    assert [r[3].get(SANDBOX_TOKEN_HEADER) for r in master.seen("POST", "/exec")] == [
        "token-1",
        "token-2",
    ]


def test_the_wrong_token_surfaces_as_a_403(master, controller):
    Sandbox("py", controller=controller)
    sb = Sandbox.connect("sb-1", "not-the-token", controller=controller)
    with pytest.raises(ControllerError) as e:
        sb.commands.run("true")
    assert e.value.status == 403


# ---- commands and eval ------------------------------------------------------


def test_a_string_runs_through_the_shell(master, controller):
    sb = Sandbox("py", controller=controller)
    r = sb.commands.run("echo ok")
    assert (r.stdout, r.stderr, r.exit_code) == ("ok\n", "", 0)
    assert master.seen("POST", "/exec")[0][2] == {
        "args": ["/bin/sh", "-c", "echo ok"],
        "timeout_secs": 30,
    }


def test_a_list_runs_directly_and_the_timeout_overrides(master, controller):
    sb = Sandbox("py", timeout=7, controller=controller)
    sb.commands.run(["echo", "ok"])
    sb.commands.run(("echo", "ok"), timeout=3)
    assert [r[2] for r in master.seen("POST", "/exec")] == [
        {"args": ["echo", "ok"], "timeout_secs": 7},
        {"args": ["echo", "ok"], "timeout_secs": 3},
    ]


def test_a_command_may_outlast_the_controllers_own_timeout(master):
    # Controller.timeout is for quick calls. A command has its own limit.
    master.exec_delay = 0.3
    quick = Controller(base_url=master.url, token=API_TOKEN, timeout=0.1)
    sb = Sandbox("py", controller=quick)
    assert sb.commands.run("sleep 0.3").exit_code == 0


def test_eval_returns_what_the_snippet_printed(master, controller):
    sb = Sandbox("py", timeout=9, controller=controller)
    assert sb.eval("print(2 + 2)") == "4\n"
    assert master.seen("POST", "/eval")[0][2] == {"code": "print(2 + 2)", "timeout_secs": 9}


@pytest.mark.parametrize("message", ["NameError: name 'x' is not defined", ""])
def test_a_failing_snippet_raises(master, controller, message):
    master.eval_reply = {"error": message, "exit_code": 1}
    sb = Sandbox("py", controller=controller)
    with pytest.raises(RuntimeError, match="sandstorm eval"):
        sb.eval("x")


# ---- ending -----------------------------------------------------------------


def test_kill_deletes_it_and_can_be_repeated(master, controller):
    sb = Sandbox("py", controller=controller)
    sb.kill()
    sb.kill()
    assert len(master.seen("DELETE", "/v1/sandboxes/sb-1")) == 1
    assert master.sandboxes == {}


def test_a_with_block_kills_it_even_on_an_error(master, controller):
    with pytest.raises(KeyError):
        with Sandbox("py", controller=controller):
            raise KeyError("boom")
    assert master.sandboxes == {}


# ---- connecting to one that exists -------------------------------------------


def test_connect_drives_an_existing_sandbox(master, controller):
    created = controller.create_sandbox(template="py")
    sb = Sandbox.connect(created["id"], created["token"], controller=controller)
    assert sb.commands.run("true").exit_code == 0
    assert len(master.seen("POST", "/v1/sandboxes")) == 1
    sb.kill()
    assert master.sandboxes == {}


def test_connect_waits_but_does_not_remove_what_it_did_not_create(master, controller):
    master.reports_until_running = None
    created = controller.create_sandbox(template="py")
    with pytest.raises(RuntimeError, match="did not start"):
        Sandbox.connect(
            created["id"], created["token"], start_timeout=0.1, controller=controller
        )
    assert created["id"] in master.sandboxes


def test_connect_to_nothing_raises(controller):
    with pytest.raises(ControllerError) as e:
        Sandbox.connect("sb-none", "t", controller=controller)
    assert e.value.status == 404


# ---- the Controller's own calls ----------------------------------------------


def test_branch_sends_the_tag_and_nothing_else(master, controller):
    sb = controller.create_sandbox(template="python")
    assert controller.branch_sandbox(sb["id"], "checkpoint-1")["tag"] == "checkpoint-1"
    assert master.seen("POST", "/branch")[0][2] == {"tag": "checkpoint-1"}


def test_branch_needs_a_tag(controller):
    with pytest.raises(TypeError):
        controller.branch_sandbox("sb-1")


def test_branch_waits_as_long_as_the_master_may_take(controller, monkeypatch):
    seen = {}

    def request(method, path, body=None, sandbox_token=None, timeout=None):
        seen["timeout"] = timeout

    monkeypatch.setattr(controller, "_request", request)
    controller.branch_sandbox("sb-1", "t")
    assert seen["timeout"] == 185


def test_spawn_sends_only_what_a_master_reads(master, controller):
    controller.spawn_sandboxes("checkpoint-1", n=2, memory_limit_mib=512)
    assert master.seen("POST", "/sandboxes")[0][2] == {
        "snapshot_tag": "checkpoint-1",
        "n": 2,
        "per_child_netns": False,
        "memory_limit_mib": 512,
    }


@pytest.mark.parametrize("removed", ["prewarm", "live_fork", "hugepages"])
def test_what_v0_6_removed_is_not_offered(controller, removed):
    with pytest.raises(TypeError):
        controller.spawn_sandboxes("checkpoint-1", **{removed: True})


def test_templates_and_touch(master, controller):
    assert controller.list_templates() == ["python"]
    sb = controller.create_sandbox(template="python")
    assert controller.touch_sandbox(sb["id"])["tier"] == "hot"
