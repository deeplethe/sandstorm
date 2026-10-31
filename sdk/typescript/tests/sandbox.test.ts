import { describe, expect, it } from "vitest";
import { Controller, SANDBOX_TOKEN_HEADER } from "../src/controller.js";
import { Sandbox } from "../src/sandbox.js";
import type { SandboxInfo } from "../src/types.js";

function info(id: string, token?: string): SandboxInfo {
  return {
    id,
    token,
    node_id: "f3",
    pool: "default",
    template: "python",
    tier: "hot",
    port: 8000,
    volume: null,
    created_at: 1,
    created_at_unix: 1,
    last_active_at: 1,
  };
}

function server(requireToken = true) {
  const calls: Request[] = [];
  const fetchMock: typeof fetch = async (input, init) => {
    const request = new Request(input, init);
    calls.push(request);
    expect(request.headers.get("authorization")).toBe("Bearer api-secret");
    const path = new URL(request.url).pathname;
    if (path === "/v1/sandboxes") {
      expect(request.headers.has(SANDBOX_TOKEN_HEADER)).toBe(false);
      const body = await request.clone().json();
      return Response.json(
        body.n === undefined ? info("a", "token-a") : [info("a", "token-a"), info("b", "token-b")],
        { status: 201 },
      );
    }
    const [, , , id, operation] = path.split("/");
    if (operation === undefined) {
      if (request.method === "DELETE") return new Response(null, { status: 204 });
      expect(request.headers.has(SANDBOX_TOKEN_HEADER)).toBe(false);
      return Response.json({ ...info(id!), ip: "10.86.0.2" });
    }
    if (operation === "touch") return Response.json(info(id!));
    if (requireToken && request.headers.get(SANDBOX_TOKEN_HEADER) !== `token-${id}`) {
      return Response.json({ error: "sandbox access denied" }, { status: 403 });
    }
    if (operation === "exec") return Response.json({ stdout: "ok", stderr: "", exit_code: 0 });
    if (operation === "eval") {
      const body = await request.clone().json();
      if (body.code === "fail") return Response.json({ error: "evaluation failed", exit_code: 1 });
      return Response.json({ result: "4\n", exit_code: 0 });
    }
    return Response.json({ ok: true, tier: "hot", latency_ms: 3 });
  };
  return { calls, options: { baseUrl: "http://test", token: "api-secret", fetch: fetchMock } };
}

describe("Sandbox credentials", () => {
  it("forwards creation tokens through all high-level operations", async () => {
    const { calls, options } = server();
    const sandbox = await Sandbox.create({ ...options, snapshotTag: "py" });
    const reaching = calls.length;
    expect(await sandbox.exec(["echo", "ok"], { timeoutSecs: 17 })).toEqual({ stdout: "ok", stderr: "", exit_code: 0 });
    expect(await sandbox.eval("print(2+2)")).toBe("4\n");
    expect(await sandbox.ping()).toEqual({ ok: true, tier: "hot", latency_ms: 3 });
    expect(await calls[reaching]!.clone().json()).toEqual({ args: ["echo", "ok"], timeout_secs: 17 });
    for (const request of calls.slice(reaching)) expect(request.headers.get(SANDBOX_TOKEN_HEADER)).toBe("token-a");
    await expect(sandbox.eval("fail")).rejects.toThrow("eval error (exit 1): evaluation failed");
  });

  it("keeps tokens separate when wrapping creation results and interleaving calls", async () => {
    const { calls, options } = server();
    const controller = new Controller(options);
    const created = await controller.spawnSandboxes({ snapshotTag: "py", n: 2 });
    const a = new Sandbox(controller, created[0]!);
    const b = new Sandbox(controller, created[1]!);
    await a.exec(["true"]);
    await b.eval("print(1)");
    await a.ping();
    await b.exec(["true"]);
    await a.eval("print(1)");
    await b.ping();
    expect(calls.slice(1).map(request => request.headers.get(SANDBOX_TOKEN_HEADER))).toEqual(["token-a", "token-b", "token-a", "token-b", "token-a", "token-b"]);
    expect(await calls[1]!.clone().json()).toEqual({ args: ["true"], timeout_secs: 30 });
  });

  it("keeps the token after the read that waits for the sandbox, which has none", async () => {
    const { calls, options } = server();
    const sandbox = await Sandbox.create({ ...options, template: "python" });
    expect(sandbox.info.ip).toBe("10.86.0.2");
    await sandbox.touch();
    await sandbox.exec(["true"]);
    expect(calls.at(-1)!.headers.get(SANDBOX_TOKEN_HEADER)).toBe("token-a");
  });

  it.each([undefined, "wrong"])("preserves server failures with token %s", async token => {
    const { options } = server();
    const sandbox = new Sandbox(new Controller(options), info("a", token));
    for (const operation of [() => sandbox.exec(["true"]), () => sandbox.eval("print(1)"), () => sandbox.ping()]) {
      await expect(operation()).rejects.toMatchObject({ name: "ControllerError", status: 403, body: { error: "sandbox access denied" } });
    }
  });

  it("omits the sandbox header without substituting the Bearer", async () => {
    const { calls, options } = server(false);
    const sandbox = new Sandbox(new Controller(options), info("a"));
    await sandbox.exec(["true"]);
    expect(await sandbox.eval("print(2+2)")).toBe("4\n");
    await sandbox.ping();
    for (const request of calls) expect(request.headers.has(SANDBOX_TOKEN_HEADER)).toBe(false);
  });
});

describe("Sandbox.create", () => {
  // A master that reports the sandbox as each of `seen` in turn.
  function master(seen: Array<Partial<SandboxInfo>>) {
    const calls: Array<{ method: string; path: string; body: unknown }> = [];
    let reads = 0;
    const fetchMock: typeof fetch = async (input, init) => {
      const url = new URL(String(input));
      const body = typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
      calls.push({ method: init?.method ?? "GET", path: url.pathname, body });
      if (init?.method === "POST") return Response.json(info("a", "token-a"), { status: 201 });
      if (init?.method === "DELETE") return new Response(null, { status: 204 });
      const reply = seen[Math.min(reads++, seen.length - 1)]!;
      return Response.json({ ...info("a"), ...reply });
    };
    return { calls, options: { baseUrl: "http://test", fetch: fetchMock } };
  }

  it("starts from a template, and sends nothing unasked", async () => {
    const { calls, options } = master([{ ip: "10.86.0.2" }]);
    await Sandbox.create({ ...options, template: "python" });
    expect(calls[0]).toEqual({ method: "POST", path: "/v1/sandboxes", body: { template: "python" } });
  });

  it("forks from a tag", async () => {
    const { calls, options } = master([{ ip: "10.86.0.2" }]);
    await Sandbox.create({ ...options, snapshotTag: "checkpoint-1", memoryLimitMib: 512 });
    expect(calls[0]!.body).toEqual({ snapshot_tag: "checkpoint-1", memory_limit_mib: 512 });
  });

  it.each([{}, { template: "python", snapshotTag: "py" }])("refuses %j", async choice => {
    const { calls, options } = master([]);
    await expect(Sandbox.create({ ...options, ...choice })).rejects.toThrow(/template or a snapshotTag/);
    expect(calls).toEqual([]);
  });

  it("waits until a node reports it running", async () => {
    const { calls, options } = master([{}, {}, { ip: "10.86.0.2" }]);
    const sandbox = await Sandbox.create({ ...options, template: "python" });
    expect(calls.filter(call => call.method === "GET")).toHaveLength(3);
    expect(sandbox.info.ip).toBe("10.86.0.2");
  });

  it("gives up on one the master has quarantined, says why, and removes it", async () => {
    const { calls, options } = master([{ last_error: "no such binary", error_attempts: 5 }]);
    await expect(Sandbox.create({ ...options, template: "python" })).rejects.toThrow(
      "failed to start: no such binary",
    );
    expect(calls.at(-1)).toMatchObject({ method: "DELETE", path: "/v1/sandboxes/a" });
  });

  it("gives up after startTimeoutMs, and removes it", async () => {
    const { calls, options } = master([{}]);
    await expect(
      Sandbox.create({ ...options, template: "python", startTimeoutMs: 10 }),
    ).rejects.toThrow(/did not start within 10 ms/);
    expect(calls.at(-1)).toMatchObject({ method: "DELETE" });
  });
});
