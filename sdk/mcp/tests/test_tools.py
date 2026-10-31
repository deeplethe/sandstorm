"""The tools against a stand-in for a master, as docs/API.md describes it."""
import json

import httpx
import pytest

from sandstorm_mcp import server


class Master:
    """Answers as a master does, reporting a sandbox as each of `seen` in turn."""

    def __init__(self, seen=({'ip': '10.86.0.2'},)):
        self.seen = list(seen)
        self.requests = []
        self.reads = 0

    def bodies(self, method):
        return [json.loads(r.content) for r in self.requests if r.method == method]

    def calls(self):
        return [f'{r.method} {r.url.path}' for r in self.requests]

    def handle(self, request):
        self.requests.append(request)
        path, method = request.url.path, request.method
        if method == 'DELETE':
            return httpx.Response(204)
        if path == '/v1/templates':
            return httpx.Response(200, json=['python'])
        if path == '/v1/sandboxes' and method == 'POST':
            n = json.loads(request.content)['n']
            return httpx.Response(201, json=[{'id': f'sb-{i}', 'token': f'token-{i}'} for i in range(n)])
        if path.endswith('/branch'):
            return httpx.Response(201, json={'tag': 'checkpoint-1', 'size_bytes': 1})
        if path.endswith('/touch'):
            return httpx.Response(200, json={'id': 'sb-0', 'tier': 'hot'})
        if path.startswith('/v1/sandboxes/'):
            reply = self.seen[min(self.reads, len(self.seen) - 1)]
            self.reads += 1
            return httpx.Response(200, json={'id': path.rsplit('/', 1)[1], **reply})
        return httpx.Response(200, json={'stdout': '', 'stderr': '', 'exit_code': 0})


@pytest.fixture
def master(monkeypatch):
    return use(monkeypatch, Master())


def use(monkeypatch, stand_in):
    client = httpx.Client
    monkeypatch.setattr(server.httpx, 'Client', lambda **kwargs: client(
        **kwargs, transport=httpx.MockTransport(stand_in.handle)))
    return stand_in


def test_a_template_starts_a_sandbox_and_sends_nothing_unasked(master):
    made = server.spawn_sandboxes(template='python')
    assert master.bodies('POST') == [{'template': 'python', 'n': 1}]
    assert made == [{'id': 'sb-0', 'token': 'token-0', 'ip': '10.86.0.2'}]


def test_a_tag_forks_with_a_memory_limit_only_when_asked(master):
    server.spawn_sandboxes(snapshot_tag='checkpoint-1', n=2, memory_limit_mib=512)
    assert master.bodies('POST') == [{'snapshot_tag': 'checkpoint-1', 'n': 2, 'memory_limit_mib': 512}]


@pytest.mark.parametrize('choice', [{}, {'template': 'python', 'snapshot_tag': 'py'}])
def test_a_template_or_a_tag_not_both_or_neither(master, choice):
    with pytest.raises(ValueError):
        server.spawn_sandboxes(**choice)
    assert master.requests == []


def test_waits_until_a_node_reports_it_running(monkeypatch):
    master = use(monkeypatch, Master([{}, {}, {'ip': '10.86.0.2'}]))
    made = server.spawn_sandboxes(template='python')
    assert master.reads == 3
    assert made[0]['token'] == 'token-0'


def test_one_the_master_quarantined_is_removed_with_its_reason(monkeypatch):
    master = use(monkeypatch, Master([{'last_error': 'no such binary', 'error_attempts': 5}]))
    with pytest.raises(RuntimeError, match='no such binary'):
        server.spawn_sandboxes(template='python', n=2)
    assert master.calls()[-2:] == ['DELETE /v1/sandboxes/sb-0', 'DELETE /v1/sandboxes/sb-1']


def test_one_that_never_starts_is_removed(monkeypatch):
    master = use(monkeypatch, Master([{}]))
    with pytest.raises(RuntimeError, match='did not start'):
        server.spawn_sandboxes(template='python', start_timeout_secs=0.01)
    assert master.calls()[-1] == 'DELETE /v1/sandboxes/sb-0'


def test_any_failure_while_waiting_removes_them(monkeypatch):
    class Garbled(Master):
        def handle(self, request):
            if request.method == 'GET':
                self.requests.append(request)
                return httpx.Response(200, text='not json')
            return super().handle(request)

    master = use(monkeypatch, Garbled())
    with pytest.raises(ValueError):
        server.spawn_sandboxes(template='python')
    assert master.calls()[-1] == 'DELETE /v1/sandboxes/sb-0'


def test_the_start_timeout_covers_the_whole_batch(monkeypatch):
    master = use(monkeypatch, Master([{}]))
    with pytest.raises(RuntimeError, match='sb-0 did not start'):
        server.spawn_sandboxes(template='python', n=3, start_timeout_secs=0.05)
    assert not any(r.url.path.endswith('/sb-1') and r.method == 'GET' for r in master.requests)


def test_a_tag_is_encoded(master):
    server.delete_snapshot('a/b?c')
    assert master.requests[-1].url.raw_path == b'/v1/snapshots/a%2Fb%3Fc'


def test_branch_sends_the_tag_and_nothing_else(master):
    assert server.branch_sandbox('sb-0', 'checkpoint-1')['tag'] == 'checkpoint-1'
    assert master.bodies('POST') == [{'tag': 'checkpoint-1'}]


def test_branch_needs_a_tag():
    with pytest.raises(TypeError):
        server.branch_sandbox('sb-0')


def test_the_other_routes(master):
    assert server.list_templates() == ['python']
    assert server.touch_sandbox('sb-0')['tier'] == 'hot'
    assert server.delete_snapshot('checkpoint-1') == {'tag': 'checkpoint-1', 'deleted': True}
    assert master.calls() == [
        'GET /v1/templates',
        'POST /v1/sandboxes/sb-0/touch',
        'DELETE /v1/snapshots/checkpoint-1',
    ]


@pytest.mark.parametrize('call, read_timeout', [
    (lambda: server.list_sandboxes(), 60),
    (lambda: server.exec_command('sb-0', ['true']), 60),
    # Longer than the master holds them, or our timeout beats its answer.
    (lambda: server.exec_command('sb-0', ['true'], timeout_secs=120), 145),
    (lambda: server.eval_code('sb-0', 'print(1)', timeout_secs=120), 145),
    (lambda: server.branch_sandbox('sb-0', 't'), 185),
])
def test_waits_as_long_as_the_master_may_take(master, monkeypatch, call, read_timeout):
    monkeypatch.setattr(server, 'HTTP_TIMEOUT', 60.0)
    call()
    assert master.requests[-1].extensions['timeout']['read'] == read_timeout


def test_eval_sends_its_time_limit(master):
    server.eval_code('sb-0', 'print(2+2)', timeout_secs=90)
    assert master.bodies('POST') == [{'code': 'print(2+2)', 'timeout_secs': 90}]
