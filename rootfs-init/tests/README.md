# `rootfs-init/tests/`

Host-runnable smoke tests for the `sandstorm-agent.py` bridge — exercises
the eval routing to a warm-up process (the JS-via-Node path) without needing
a real Firecracker round-trip.

## Files

- `fake-warmup.py` — minimal warmup process that speaks the
  bridge protocol (line-JSON over stdin/stdout). Sends
  `{"ready": true}` then echoes any `{"id", "code"}` request back as
  `{"id", "result": "echoed: <code>"}`. Reference implementation of
  the protocol.
- `smoke-test.sh` — runs the agent against the fake warmup on the
  host, sends a `ping` + an `eval` action over TCP, prints the
  responses. Verifies the agent's bridge plumbing end-to-end.

## Run

```bash
# Bridge smoke (agent + fake warmup, requires nc + Python 3 + a free port 8888):
scp sandstorm-agent.py tests/fake-warmup.py tests/smoke-test.sh dev-box:/tmp/
ssh dev-box "bash /tmp/smoke-test.sh"
```

Expected output of `smoke-test.sh`:

```
agent_lang: "node", warmup_ready: true        ← ping response
result_json: "\"echoed: ...\""                 ← eval routed via bridge
```

Neither smoke test is wired into CI yet — they're tools for verifying
the bridge while iterating on `sandstorm-agent.py` or a new recipe.
