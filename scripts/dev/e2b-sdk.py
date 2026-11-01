#!/usr/bin/env python3
"""The official e2b SDK against a Sandstorm master, unchanged.

Run with only E2B_DOMAIN and E2B_API_KEY set (and SSL_CERT_FILE pointing at
the deployment's ca.pem when the front door uses Sandstorm's own CA):

    E2B_DOMAIN=172-21-16-4.sslip.io E2B_API_KEY=$(sudo cat .../pki/api-token) \
    SSL_CERT_FILE=/tmp/ca.pem python3 scripts/dev/e2b-sdk.py

Each check prints PASS or FAIL; the exit status is the number of failures.
"""

import io
import os
import sys
import time
import traceback

import httpx
from e2b import CommandExitException, PtySize, Sandbox, SandboxQuery, SandboxState

TEMPLATE = os.environ.get("TEMPLATE", "base")
results = []


def check(name, fn):
    t0 = time.time()
    try:
        got = fn()
        results.append((name, True))
        print(f"PASS  {name}  ({time.time() - t0:.2f}s)  {str(got)[:160]}")
        return got
    except Exception as e:  # noqa: BLE001 - every failure is reported, none stops the run
        results.append((name, False))
        print(f"FAIL  {name}  ({time.time() - t0:.2f}s)  {e!r}")
        traceback.print_exc(limit=3)
        return None


sb = check("Sandbox.create", lambda: Sandbox.create(TEMPLATE, timeout=600, envs={"GREETING": "hi"}, metadata={"suite": "e2b-sdk"}))
if sb is None:
    sys.exit(1)
sid = sb.sandbox_id


def info():
    i = sb.get_info()
    assert i.sandbox_id == sid and i.state == SandboxState.RUNNING, i
    assert i.metadata.get("suite") == "e2b-sdk", i.metadata
    return f"state={i.state.value} started={i.started_at} end={i.end_at}"


check("get_info", info)


def listed():
    p = Sandbox.list(query=SandboxQuery(metadata={"suite": "e2b-sdk"}))
    ids = [s.sandbox_id for s in p.next_items()]
    assert sid in ids, ids
    return ids


check("list (filtered by metadata)", listed)


def timeout():
    before = sb.get_info().end_at
    sb.set_timeout(900)
    after = sb.get_info().end_at
    assert after > before, (before, after)
    return f"{before} -> {after}"


check("set_timeout", timeout)
check("commands.run id runs as user", lambda: (lambda o: o if "uid=1000(user)" in o else (_ for _ in ()).throw(AssertionError(o)))(sb.commands.run("id").stdout.strip()))
check("envs from create reach the sandbox", lambda: (lambda o: o if o == "hi" else (_ for _ in ()).throw(AssertionError(o)))(sb.commands.run("echo $GREETING").stdout.strip()))


def streamed():
    seen = []
    t0 = time.time()
    sb.commands.run(
        "for i in 1 2 3; do echo tick$i; sleep 1; done",
        on_stdout=lambda d: seen.append((round(time.time() - t0, 1), d.strip())),
    )
    assert [s for _, s in seen] == ["tick1", "tick2", "tick3"], seen
    # Arrived as written, not all at the end.
    assert seen[0][0] < 1.0 and seen[2][0] - seen[0][0] >= 1.5, seen
    return seen


check("commands.run streams", streamed)


def exit_code():
    try:
        sb.commands.run("exit 7")
    except CommandExitException as e:
        assert e.exit_code == 7
        return e.exit_code
    raise AssertionError("no exception for a failing command")


check("commands.run exit code", exit_code)


def background():
    h = sb.commands.run("sleep 60", background=True)
    assert h.pid in [p.pid for p in sb.commands.list()]
    assert sb.commands.kill(h.pid)
    return h.pid


check("background command, list, kill", background)


def pty():
    out = []
    h = sb.pty.create(PtySize(rows=24, cols=80))
    sb.pty.send_stdin(h.pid, b"echo pty-$((6*7)); tty; exit\n")
    h.wait(on_pty=lambda d: out.append(d))
    text = b"".join(out).decode(errors="replace")
    assert "pty-42" in text and "/dev/pts/" in text, text
    return text.replace("\r", "")[-60:]


check("pty", pty)
check("files.write + read", lambda: (sb.files.write("/home/user/hello.txt", "hello from e2b"), sb.files.read("/home/user/hello.txt"))[1])
payload = os.urandom(4 * 1024 * 1024)


def upload_download():
    sb.files.write("/home/user/blob.bin", io.BytesIO(payload))
    got = sb.files.read("/home/user/blob.bin", format="bytes")
    assert bytes(got) == payload, len(got)
    return f"{len(got)} bytes round-tripped"


check("files upload + download (4 MiB)", upload_download)


def signed_urls():
    up = sb.upload_url("/home/user/via-url.txt")
    r = httpx.post(up, files={"file": ("via-url.txt", b"by url")}, timeout=30)
    assert r.status_code < 300, (r.status_code, r.text)
    down = sb.download_url("/home/user/via-url.txt")
    r = httpx.get(down, timeout=30)
    assert r.status_code == 200 and r.content == b"by url", (r.status_code, r.text)
    return "upload_url/download_url ok"


check("upload_url / download_url", signed_urls)
check("files.list", lambda: sorted(e.name for e in sb.files.list("/home/user") if not e.name.startswith(".")))


def watch():
    w = sb.files.watch_dir("/home/user")
    time.sleep(0.5)
    sb.files.write("/home/user/watched.txt", "x")
    time.sleep(1.5)
    ev = [(e.type.name, e.name) for e in w.get_new_events()]
    w.stop()
    assert any(n == "watched.txt" for _, n in ev), ev
    return ev


check("files.watch_dir", watch)


def user_port():
    sb.commands.run("python3 -m http.server 8000 --directory /home/user", background=True)
    url = f"https://{sb.get_host(8000)}/hello.txt"
    for _ in range(50):
        try:
            r = httpx.get(url, timeout=5)
            if r.status_code == 200:
                assert r.text == "hello from e2b", r.text
                return url
        except httpx.HTTPError:
            pass
        time.sleep(0.2)
    raise AssertionError(f"{url} never answered")


check("get_host(8000) reaches a server in the sandbox", user_port)


PAUSE_SETTLE = float(os.environ.get("PAUSE_SETTLE", "25"))


def pause():
    # In memory only: if it is there after the resume, the sandbox was
    # restored from its checkpoint rather than started again.
    sb.commands.run("echo still-here > /dev/shm/marker")
    sb.beta_pause()
    i = Sandbox.get_info(sid)
    assert i.state == SandboxState.PAUSED, i.state
    # The node holds a demotion back while recent traffic drains; wait it
    # out so the resume below is from a real checkpoint.
    time.sleep(PAUSE_SETTLE)
    i = Sandbox.get_info(sid)
    assert i.state == SandboxState.PAUSED, i.state
    return i.state.value


check("beta_pause", pause)


def connect():
    s2 = Sandbox.connect(sid, timeout=300)
    assert Sandbox.get_info(sid).state == SandboxState.RUNNING
    # Memory survived: the file and the environment are still there.
    assert s2.files.read("/home/user/hello.txt") == "hello from e2b"
    assert s2.commands.run("cat /dev/shm/marker").stdout.strip() == "still-here"
    return s2.commands.run("echo $GREETING; id -un").stdout.split()


check("connect resumes a paused sandbox", connect)
check("kill", lambda: sb.kill())


def gone():
    try:
        Sandbox.get_info(sid)
    except Exception as e:  # NotFoundException
        return type(e).__name__
    raise AssertionError("still there after kill")


check("killed sandbox is gone", gone)


def again():
    # Each takes the port the last one gave up, which a client's kept
    # connection still points at.
    for _ in range(6):
        s = Sandbox.create(TEMPLATE, timeout=60, request_timeout=20)
        assert s.commands.run("echo ok").stdout.strip() == "ok"
        s.kill()
    return "6 in a row"


check("create straight after a kill", again)

failed = [n for n, ok in results if not ok]
print(f"\n{len(results) - len(failed)}/{len(results)} passed" + (f"; failed: {failed}" if failed else ""))
sys.exit(len(failed))
