#!/usr/bin/env python3
"""Create to first answer, one sandbox at a time, polled every 50 ms.

The same method as baselines.py: start one, poll until it answers 200,
record the time, remove it, start the next. Run on the node itself so the
gateway is reached over loopback.

    sudo python3 starttime.py --master https://127.0.0.1:18080 \
        --pki /var/lib/sst-time/pki --templates web,app64 --count 20 --out st.jsonl
"""

import argparse
import json
import ssl
import time
import urllib.error
import urllib.request
from pathlib import Path


class Api:
    def __init__(self, master, pki):
        self.master = master
        self.token = (Path(pki) / "api-token").read_text().strip()
        self.ctx = ssl.create_default_context(cafile=str(Path(pki) / "ca.pem"))
        self.ctx.check_hostname = False

    def call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.master + path, data=data, method=method, headers={
            "authorization": f"Bearer {self.token}", "content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, context=self.ctx, timeout=60) as r:
                raw = r.read()
                return r.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()[:300]


def answer(url, token, deadline):
    req = urllib.request.Request(url, headers={"x-sandstorm-sandbox-token": token})
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(req, timeout=2) as r:
                r.read()
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.05)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--master", required=True)
    ap.add_argument("--pki", required=True)
    ap.add_argument("--templates", required=True)
    ap.add_argument("--count", type=int, default=20)
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    api = Api(a.master, a.pki)
    out = open(a.out, "a", buffering=1)
    for template in a.templates.split(","):
        for n in range(a.count):
            t0 = time.perf_counter()
            st, sb = api.call("POST", "/v1/sandboxes", {"template": template, "port": a.port})
            accepted = time.perf_counter() - t0
            if st not in (200, 201):
                out.write(json.dumps({"template": template, "n": n, "error": f"{st} {sb}"}) + "\n")
                continue
            # The gateway port is known at placement; poll the gateway itself,
            # which answers once the node has the sandbox up and the app listens.
            while not sb.get("gateway_port"):
                time.sleep(0.05)
                _, sb = api.call("GET", f"/v1/sandboxes/{sb['id']}")
            ok = answer(f"http://127.0.0.1:{sb['gateway_port']}/", sb["token"], time.time() + 120)
            total = time.perf_counter() - t0
            row = {"t": round(time.time(), 3), "template": template, "n": n, "id": sb["id"],
                   "ok": ok, "accepted_s": round(accepted, 4), "create_to_answer_s": round(total, 4)}
            out.write(json.dumps(row) + "\n")
            print(template, n, "ok" if ok else "FAILED", f"{total * 1000:.0f} ms", flush=True)
            api.call("DELETE", f"/v1/sandboxes/{sb['id']}")
            time.sleep(1.5)


if __name__ == "__main__":
    main()
