import json

import httpx
import pytest

from sandstorm_mcp import server


@pytest.fixture
def requests(monkeypatch):
    recorded = []
    client = httpx.Client

    def handle(request):
        recorded.append(request)
        assert request.headers['Authorization'] == 'Bearer api-token'
        assert request.extensions['timeout']['read'] == server.HTTP_TIMEOUT
        token = request.headers.get('x-sandstorm-sandbox-token')
        if token:
            assert token not in str(request.url)
            assert token not in request.content.decode()
        if request.url.path.endswith(('/exec', '/eval', '/ping')):
            sandbox_id = request.url.path.split('/')[3]
            if request.headers.get('x-sandstorm-sandbox-token') != f'{sandbox_id}-token':
                return httpx.Response(401, json={'error': 'unauthorized'})
            count = sum(r.url.path == request.url.path for r in recorded)
            return httpx.Response(200, json={'stdout': 'ready' if count > 1 else 'pending',
                                           'stderr': '', 'exit_code': 0, 'result': '4'})
        assert 'x-sandstorm-sandbox-token' not in request.headers
        return httpx.Response(200, json=[])

    monkeypatch.setattr(server, 'SANDSTORM_TOKEN', 'api-token')
    monkeypatch.setattr(server.httpx, 'Client', lambda **kwargs: client(
        **kwargs, transport=httpx.MockTransport(handle)))
    return recorded


def call_tool(name, sandbox_id, token):
    kwargs = {'sandbox_id': sandbox_id, 'sandbox_token': token}
    if name == 'exec_command':
        kwargs.update(args=['echo', 'hello'], timeout_secs=17)
    elif name == 'eval_code':
        kwargs['code'] = '2+2'
    elif name == 'wait_for_text':
        kwargs.update(path='/tmp/output', marker='ready', poll_interval_ms=0)
    return getattr(server, name)(**kwargs)


TOOLS = ['exec_command', 'eval_code', 'ping_sandbox', 'wait_for_text']


@pytest.mark.parametrize('name', TOOLS)
def test_tokens_are_per_call(name, requests):
    for sandbox_id in ['a', 'b', 'a']:
        result = call_tool(name, sandbox_id, f'{sandbox_id}-token')
        if name == 'wait_for_text':
            assert result['found'] is True
            assert result['last_excerpt'] == 'ready'
        else:
            assert result['exit_code'] == 0
            assert result['result'] == '4'
    for request in requests:
        sandbox_id = request.url.path.split('/')[3]
        assert request.headers['x-sandstorm-sandbox-token'] == f'{sandbox_id}-token'
        if name == 'exec_command':
            assert json.loads(request.content) == {'args': ['echo', 'hello'], 'timeout_secs': 17}
        elif name == 'wait_for_text':
            assert json.loads(request.content)['timeout_secs'] == 5
        elif name == 'eval_code':
            assert json.loads(request.content) == {'code': '2+2', 'timeout_secs': 30}
    if name == 'wait_for_text':
        assert len(requests) == 5
    server.list_sandboxes()
    server.get_sandbox('a')


@pytest.mark.parametrize('name', TOOLS)
@pytest.mark.parametrize('token', [None, 'wrong-token'])
def test_authentication_failure_is_preserved(name, token, requests):
    with pytest.raises(httpx.HTTPStatusError) as error:
        call_tool(name, 'a', token)
    assert error.value.response.status_code == 401
    assert len(requests) == 1
    if token is None:
        assert 'x-sandstorm-sandbox-token' not in requests[0].headers


def test_legacy_server_without_token(monkeypatch):
    client = httpx.Client
    recorded = []

    def handle(request):
        recorded.append(request)
        assert 'x-sandstorm-sandbox-token' not in request.headers
        return httpx.Response(200, json={'stdout': 'ready', 'exit_code': 0})

    monkeypatch.setattr(server.httpx, 'Client', lambda **kwargs: client(
        **kwargs, transport=httpx.MockTransport(handle)))
    assert server.exec_command('a', ['true'])['exit_code'] == 0
    assert json.loads(recorded[-1].content)['timeout_secs'] == 30
    assert server.eval_code('a', '1')['exit_code'] == 0
    assert server.ping_sandbox('a')['exit_code'] == 0
    assert server.wait_for_text('a', '/tmp/output', 'ready')['found'] is True
