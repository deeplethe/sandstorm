"""The server against a real TLS listener under a private CA.

A master signs its certificate with a CA of its own, so these are what stands
between the API token and anything else that answers on its address.
"""
import json
import os
import ssl
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import pytest

from sandstorm_mcp import server

# The same shape as a master's own PKI: a private CA over a leaf for
# 127.0.0.1, ECDSA, with each authority key id set.
OPENSSL_CONFIG = """
[req]
distinguished_name = dn
[dn]
[ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[leaf]
subjectAltName = IP:127.0.0.1, DNS:localhost
extendedKeyUsage = serverAuth
authorityKeyIdentifier = keyid
"""


def openssl(cwd, *args):
    subprocess.run(['openssl', *args], cwd=cwd, check=True, stdin=subprocess.DEVNULL,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def make_key(cwd, name):
    openssl(cwd, 'ecparam', '-name', 'prime256v1', '-genkey', '-noout', '-out', f'{name}-key.pem')


def make_ca(cwd, name):
    make_key(cwd, name)
    openssl(cwd, 'req', '-new', '-x509', '-key', f'{name}-key.pem', '-sha256',
            '-config', 'openssl.cnf', '-extensions', 'ca', '-subj', f'/CN={name}',
            '-days', '2', '-out', f'{name}.pem')


def issue_leaf(cwd, ca):
    make_key(cwd, 'leaf')
    openssl(cwd, 'req', '-new', '-key', 'leaf-key.pem', '-sha256', '-config', 'openssl.cnf',
            '-subj', '/CN=sandstorm master', '-out', 'leaf.csr')
    openssl(cwd, 'x509', '-req', '-in', 'leaf.csr', '-CA', f'{ca}.pem', '-CAkey', f'{ca}-key.pem',
            '-CAcreateserial', '-sha256', '-days', '2', '-extfile', 'openssl.cnf',
            '-extensions', 'leaf', '-out', 'leaf.pem')


@pytest.fixture(scope='module')
def pki(tmp_path_factory):
    path = tmp_path_factory.mktemp('pki')
    (path / 'openssl.cnf').write_text(OPENSSL_CONFIG)
    make_ca(path, 'ca')
    make_ca(path, 'other-ca')
    issue_leaf(path, 'ca')
    return path


@pytest.fixture
def master(pki):
    """A stand-in for a master, recording the tokens that reach it."""
    tokens = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            tokens.append(self.headers.get('Authorization'))
            body = json.dumps([{'tag': 'py'}]).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    httpd.handle_error = lambda *args: None  # a refused handshake is the point
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain(pki / 'leaf.pem', pki / 'leaf-key.pem')
    httpd.socket = context.wrap_socket(httpd.socket, server_side=True, do_handshake_on_connect=False)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    httpd.tokens = tokens
    httpd.url = f'https://127.0.0.1:{httpd.server_address[1]}'
    yield httpd
    httpd.shutdown()
    httpd.server_close()


def point_at(monkeypatch, url, ca):
    monkeypatch.setattr(server, 'SANDSTORM_URL', url)
    monkeypatch.setattr(server, 'SANDSTORM_CA', ca and str(ca))
    monkeypatch.setattr(server, 'SANDSTORM_TOKEN', 'api-token')
    monkeypatch.setattr(server, 'SSL_CONTEXT', server._ssl_context())


def test_the_default_address_is_where_a_master_listens():
    env = {k: v for k, v in os.environ.items() if k not in ('SANDSTORM_URL', 'SANDSTORM_CA')}
    out = subprocess.run(
        [sys.executable, '-c', 'from sandstorm_mcp import server; print(server.SANDSTORM_URL)'],
        env=env, capture_output=True, text=True, check=True)
    assert out.stdout.strip() == 'https://127.0.0.1:8080'


def test_verifies_against_the_ca(master, pki, monkeypatch):
    point_at(monkeypatch, master.url, pki / 'ca.pem')
    assert server.list_snapshots() == [{'tag': 'py'}]
    assert master.tokens == ['Bearer api-token']


def test_refuses_with_no_ca_before_the_token_is_sent(master, monkeypatch):
    point_at(monkeypatch, master.url, None)
    with pytest.raises(httpx.ConnectError, match='CERTIFICATE_VERIFY_FAILED'):
        server.list_snapshots()
    assert master.tokens == []


def test_refuses_a_master_another_ca_signed(master, pki, monkeypatch):
    point_at(monkeypatch, master.url, pki / 'other-ca.pem')
    with pytest.raises(httpx.ConnectError, match='CERTIFICATE_VERIFY_FAILED'):
        server.list_snapshots()
    assert master.tokens == []


def test_a_missing_ca_stops_the_server_rather_than_being_skipped(monkeypatch, tmp_path):
    monkeypatch.setattr(server, 'SANDSTORM_URL', 'https://127.0.0.1:8080')
    monkeypatch.setattr(server, 'SANDSTORM_CA', str(tmp_path / 'absent.pem'))
    with pytest.raises(OSError):
        server._ssl_context()


def test_an_empty_ca_is_refused(monkeypatch, tmp_path):
    empty = tmp_path / 'empty.pem'
    empty.write_text('')
    monkeypatch.setattr(server, 'SANDSTORM_URL', 'https://127.0.0.1:8080')
    monkeypatch.setattr(server, 'SANDSTORM_CA', str(empty))
    with pytest.raises(ssl.SSLError):
        server._ssl_context()


def test_a_plain_http_master_does_not_read_the_ca(monkeypatch, tmp_path):
    # As the Python Controller: the CA only means anything over https.
    monkeypatch.setattr(server, 'SANDSTORM_URL', 'http://127.0.0.1:8889')
    monkeypatch.setattr(server, 'SANDSTORM_CA', str(tmp_path / 'absent.pem'))
    assert server._ssl_context() is None
