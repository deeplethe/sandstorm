import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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

describe("sandbox readiness deadline", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function master(
    read: (signal: AbortSignal) => Promise<Response>,
    cleanup: () => Promise<Response> = async () => new Response(null, { status: 204 }),
  ) {
    const methods: string[] = [];
    const deleted: string[] = [];
    const fetchMock: typeof fetch = async (input, init) => {
      methods.push(init!.method!);
      if (init!.method === "POST") return Response.json(info("a", "token-a"), { status: 201 });
      if (init!.method === "DELETE") {
        deleted.push(new URL(String(input)).pathname);
        return cleanup();
      }
      return read(init!.signal!);
    };
    return { methods, deleted, options: { baseUrl: "http://test", fetch: fetchMock } };
  }

  function waitForAbort(signal: AbortSignal): Promise<Response> {
    return new Promise((_resolve, reject) => {
      signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    });
  }

  it("rejects a ready response received after the deadline", async () => {
    const { methods, options } = master(async () => {
      // This fetch returns a late response even after its signal was aborted.
      await new Promise((resolve) => setTimeout(resolve, 101));
      return Response.json({ ...info("a"), ip: "10.86.0.2" });
    });
    const sandbox = new Sandbox(new Controller(options), info("a"));
    const result = sandbox.waitUntilRunning(100).catch((error: unknown) => error);

    await vi.advanceTimersByTimeAsync(101);
    expect(await result).toMatchObject({ message: /did not start within 100 ms/ });
    expect(sandbox.info.ip).toBeUndefined();
    expect(methods).toEqual(["GET"]);
  });

  it.each([
    {
      response: () => Response.json({ error: "missing sandbox" }, { status: 404 }),
      error: { name: "ControllerError", status: 404 },
    },
    {
      response: () => new Response("not JSON"),
      error: { name: "SyntaxError" },
    },
  ])("preserves a late status failure with $error.name", async ({ response, error }) => {
    const { methods, options } = master(async () => {
      await new Promise((resolve) => setTimeout(resolve, 101));
      return response();
    });
    const sandbox = new Sandbox(new Controller(options), info("a"));
    const result = sandbox.waitUntilRunning(100).catch((failure: unknown) => failure);

    await vi.advanceTimersByTimeAsync(101);
    expect(await result).toMatchObject(error);
    expect(methods).toEqual(["GET"]);
  });

  it("bounds a later request by the remaining readiness budget", async () => {
    const reads: number[] = [];
    const { methods, options } = master(async (signal) => {
      reads.push(performance.now());
      if (reads.length === 1) return Response.json({ ...info("a"), last_error: "still starting" });
      return waitForAbort(signal);
    });
    const sandbox = new Sandbox(new Controller({ ...options, timeoutMs: 1000 }), info("a"));
    const onTimeout = vi.fn();
    const result = sandbox.waitUntilRunning(120).catch(onTimeout);

    await vi.advanceTimersByTimeAsync(120);
    expect(reads).toEqual([0, 50]);
    expect(methods).toEqual(["GET", "GET"]);
    expect(onTimeout).toHaveBeenCalledWith(new Error("sandbox a did not start within 120 ms; last error: still starting"));
    await result;
  });

  it("uses a smaller request timeout and keeps its original error", async () => {
    const { methods, options } = master(waitForAbort);
    const sandbox = new Sandbox(new Controller({ ...options, timeoutMs: 25 }), info("a"));
    const result = sandbox.waitUntilRunning(100).catch((error: unknown) => error);

    await vi.advanceTimersByTimeAsync(25);
    expect(await result).toMatchObject({ name: "AbortError" });
    expect(methods).toEqual(["GET"]);
  });

  it("uses the remaining budget for backoff without starting another request at the deadline", async () => {
    const { methods, options } = master(async () => Response.json(info("a")));
    const sandbox = new Sandbox(new Controller(options), info("a"));
    const onTimeout = vi.fn();
    const result = sandbox.waitUntilRunning(35).catch(onTimeout);

    await vi.advanceTimersByTimeAsync(34);
    expect(onTimeout).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    await result;
    expect(onTimeout).toHaveBeenCalledWith(new Error("sandbox a did not start within 35 ms; last error: no reason recorded"));
    expect(methods).toEqual(["GET"]);
  });

  it("deletes the sandbox owned by create and preserves the readiness error if cleanup fails", async () => {
    const { methods, deleted, options } = master(waitForAbort, async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
      return Response.json({ error: "cleanup failed" }, { status: 500 });
    });
    const onTimeout = vi.fn();
    const result = Sandbox.create({
      ...options,
      template: "python",
      startTimeoutMs: 100,
      timeoutMs: 1000,
    }).catch(onTimeout);

    await vi.advanceTimersByTimeAsync(100);
    expect(methods).toEqual(["POST", "GET", "DELETE"]);
    expect(deleted).toEqual(["/v1/sandboxes/a"]);
    expect(onTimeout).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(20);
    await result;
    expect(onTimeout).toHaveBeenCalledWith(new Error("sandbox a did not start within 100 ms; last error: no reason recorded"));
  });
});
