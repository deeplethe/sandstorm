#!/usr/bin/env python3
"""CubeSandbox's snapshot and clone, measured as fork.py measures Sandstorm's.

    python3 fork_cube.py <app64 template id>

The source is an app64 started from its template. Then: snapshot it
(`POST /sandboxes/{id}/snapshots`) 6 times, the first dropped; start one
from the snapshot to its first answer, 10 times after one; and clone n the
way its SDK's `Sandbox.clone(n, concurrency=n)` does -- one snapshot, then n
creates at once -- until every child answers, 3 rounds. v0.7.3 has no
`/fork` endpoint. Polled every 5 ms through cube-proxy.
"""

import json, statistics, sys, time, urllib.request, urllib.error, concurrent.futures as cf
API = "http://127.0.0.1:3000"; TPL = sys.argv[1]
H = {"X-API-Key": "e2b_000000", "Content-Type": "application/json"}
def call(m, path, body=None, timeout=180):
    r = urllib.request.Request(API + path, data=json.dumps(body).encode() if body is not None else None, method=m, headers=H)
    try:
        with urllib.request.urlopen(r, timeout=timeout) as f: return f.status, json.loads(f.read() or b"null")
    except urllib.error.HTTPError as e: return e.code, e.read().decode()[:300]
def answer(sid, deadline):
    req = urllib.request.Request("http://127.0.0.1/", headers={"Host": f"8000-{sid}.cube.app"})
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(req, timeout=2) as r:
                if r.status == 200: return time.time()
        except Exception: time.sleep(0.005)
    return None
def q(v): v = sorted(v); return f"p50 {statistics.median(v):.0f} ms  min {v[0]:.0f}  max {v[-1]:.0f}  n={len(v)}"
st, parent = call("POST", "/sandboxes", {"templateID": TPL, "timeout": 3600}); assert st == 201, (st, parent)
pid = parent["sandboxID"]; assert answer(pid, time.time() + 60), "parent never answered"; print("parent up", pid)
snap, snaps = [], []
for i in range(6):
    t = time.time(); st, info = call("POST", f"/sandboxes/{pid}/snapshots", {}); assert st in (200, 201), (st, info)
    snap.append((time.time() - t) * 1000); snaps.append(info["snapshotID"])
    assert answer(pid, time.time() + 30)
print("snapshot ms", [round(x) for x in snap]); print("snapshot (a running app64):", q(snap[1:]))
one = []
for i in range(11):
    t = time.time(); st, sb = call("POST", "/sandboxes", {"templateID": snaps[-1], "timeout": 3600}); assert st == 201, (st, sb)
    done = answer(sb["sandboxID"], time.time() + 60); assert done, "child never answered"; one.append((done - t) * 1000)
    call("DELETE", f"/sandboxes/{sb['sandboxID']}"); time.sleep(0.5)
print("start 1 from the snapshot, to first answer:", q(one[1:]))
def create(snap):
    st, sb = call("POST", "/sandboxes", {"templateID": snap, "timeout": 3600})
    return sb["sandboxID"] if st == 201 else None
for n in (1, 10, 20):
    walls = []
    for r in range(3):
        t = time.time(); st, info = call("POST", f"/sandboxes/{pid}/snapshots", {}); assert st in (200, 201), (st, info)
        snaps.append(info["snapshotID"])
        with cf.ThreadPoolExecutor(n) as ex:
            ids = [i for i in ex.map(lambda _: create(info["snapshotID"]), range(n)) if i]
            ends = list(ex.map(lambda s: answer(s, time.time() + 120), ids))
        ok = [e for e in ends if e]; walls.append((max(ok) - t) * 1000 if ok else float("nan"))
        if len(ok) < n: print(f"  n={n}: only {len(ok)} answered")
        for s_ in ids: call("DELETE", f"/sandboxes/{s_}")
        time.sleep(3)
    print(f"clone {n} (snapshot + {n} creates at once), until all answer: wall p50 {statistics.median(walls):.0f} ms, per child {statistics.median(walls)/n:.1f} ms  (rounds {[round(w) for w in walls]})")
call("DELETE", f"/sandboxes/{pid}")
for sid in snaps: call("DELETE", f"/snapshots/{sid}")
