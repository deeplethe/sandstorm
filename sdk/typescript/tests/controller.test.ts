import { describe, expect, it, vi } from "vitest";
import { Controller, ControllerError } from "../src/controller.js";

/**
 * Mock fetch helper. Returns a fetch impl that records calls and
 * replies with the given status + body for the next request.
 */
function mockFetch(replies: Array<{ status: number; body: unknown }>) {
  const calls: Array<{ url: string; init: RequestInit | undefined }> = [];
  let i = 0;
  const f: typeof fetch = (async (input, init) => {
    calls.push({ url: String(input), init });
    const reply = replies[i++];
    if (!reply) {
      throw new Error(`unexpected fetch call: ${input}`);
    }
    const text = typeof reply.body === "string" ? reply.body : JSON.stringify(reply.body);
    return {
      ok: reply.status >= 200 && reply.status < 300,
      status: reply.status,
      text: async () => text,
    } as Response;
  }) as typeof fetch;
  return { fetch: f, calls };
}

describe("Controller", () => {
  it("listSnapshots returns parsed JSON", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 200, body: [{ tag: "py", dir: "/x", created_at_unix: 1 }] },
    ]);
    const c = new Controller({ baseUrl: "http://test/", fetch: f });
    const result = await c.listSnapshots();
    expect(result).toEqual([{ tag: "py", dir: "/x", created_at_unix: 1 }]);
    expect(calls[0]!.url).toBe("http://test/v1/snapshots");
    expect((calls[0]!.init as RequestInit).method).toBe("GET");
  });

  it("createSandbox starts from a template, and sends nothing unasked", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 201, body: { id: "sb-1", token: "t" } }]);
    const c = new Controller({ fetch: f });
    const sb = await c.createSandbox({ template: "python" });
    expect(sb.token).toBe("t");
    expect(calls[0]!.url).toMatch(/\/v1\/sandboxes$/);
    expect(JSON.parse((calls[0]!.init as RequestInit).body as string)).toEqual({ template: "python" });
  });

  it("createSandbox serializes camelCase → snake_case", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 201, body: { id: "sb-1" } }]);
    const c = new Controller({ fetch: f });
    await c.createSandbox({
      snapshotTag: "py",
      id: "mine",
      pool: "default",
      nodeId: "f3",
      backend: "gvisor",
      port: 8000,
      volume: "data",
      memoryLimitMib: 512,
    });
    expect(JSON.parse((calls[0]!.init as RequestInit).body as string)).toEqual({
      snapshot_tag: "py",
      id: "mine",
      pool: "default",
      node_id: "f3",
      backend: "gvisor",
      port: 8000,
      volume: "data",
      memory_limit_mib: 512,
    });
  });

  it("spawnSandboxes asks for a list, from a tag or a template", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 201, body: [{ id: "sb-1", snapshot_tag: "py" }] },
      { status: 201, body: [{ id: "sb-2" }] },
    ]);
    const c = new Controller({ fetch: f });
    await c.spawnSandboxes({ snapshotTag: "py", n: 3, perChildNetns: true, memoryLimitMib: 512 });
    await c.spawnSandboxes({ template: "python" });
    expect(JSON.parse((calls[0]!.init as RequestInit).body as string)).toEqual({
      snapshot_tag: "py",
      n: 3,
      per_child_netns: true,
      memory_limit_mib: 512,
    });
    expect(JSON.parse((calls[1]!.init as RequestInit).body as string)).toEqual({
      template: "python",
      n: 1,
      per_child_netns: false,
    });
  });

  it("branchSandbox sends the tag and nothing else", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 201, body: { tag: "b1", dir: "tags/b1", created_at_unix: 1 } },
    ]);
    const c = new Controller({ fetch: f });
    await c.branchSandbox("sb-1", { tag: "b1" });
    expect((calls[0]!.init as RequestInit).method).toBe("POST");
    expect(calls[0]!.url).toMatch(/\/v1\/sandboxes\/sb-1\/branch$/);
    expect(JSON.parse((calls[0]!.init as RequestInit).body as string)).toEqual({ tag: "b1" });
  });

  it("reaches the other routes", async () => {
    const replies = Array.from({ length: 5 }, () => ({ status: 200, body: {} }));
    const { fetch: f, calls } = mockFetch(replies);
    const c = new Controller({ baseUrl: "http://x", fetch: f });
    await c.listTemplates();
    await c.getSnapshot("a/b");
    await c.touchSandbox("sb-1");
    await c.setSandboxTier("sb-1", "snapshot");
    await c.retrySandbox("sb-1");
    expect(calls.map((call) => `${(call.init as RequestInit).method} ${call.url}`)).toEqual([
      "GET http://x/v1/templates",
      "GET http://x/v1/snapshots/a%2Fb/info",
      "POST http://x/v1/sandboxes/sb-1/touch",
      "POST http://x/v1/sandboxes/sb-1/tier",
      "POST http://x/v1/sandboxes/sb-1/retry",
    ]);
    expect(JSON.parse((calls[3]!.init as RequestInit).body as string)).toEqual({ tier: "snapshot" });
  });

  it("evalCode sends its time limit", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 200, body: { result: "4\n", exit_code: 0 } }]);
    const c = new Controller({ fetch: f });
    await c.evalCode("sb-1", "print(2+2)", "t", 90);
    expect(JSON.parse((calls[0]!.init as RequestInit).body as string)).toEqual({
      code: "print(2+2)",
      timeout_secs: 90,
    });
  });

  describe("waiting as long as the master may take", () => {
    // Whatever timer the request set, without waiting for it.
    async function timerFor(call: (c: Controller) => Promise<unknown>) {
      const set = vi.spyOn(globalThis, "setTimeout");
      const { fetch: f } = mockFetch([{ status: 200, body: {} }]);
      await call(new Controller({ fetch: f, timeoutMs: 60_000 }));
      const delay = set.mock.calls[0]![1];
      set.mockRestore();
      return delay;
    }

    it("keeps the default for a short call", async () => {
      expect(await timerFor((c) => c.listSnapshots())).toBe(60_000);
      expect(await timerFor((c) => c.execCommand("sb-1", ["true"]))).toBe(60_000);
    });

    it("outlasts a long exec or eval, which the master holds for 20 s more", async () => {
      expect(await timerFor((c) => c.execCommand("sb-1", ["true"], { timeoutSecs: 120 }))).toBe(145_000);
      expect(await timerFor((c) => c.evalCode("sb-1", "1", "t", 120))).toBe(145_000);
    });

    it("outlasts a branch, which the master gives up on at 180 s", async () => {
      expect(await timerFor((c) => c.branchSandbox("sb-1", { tag: "t" }))).toBe(185_000);
    });
  });

  it("raises ControllerError on non-2xx with JSON body", async () => {
    // Two replies because we call getSandbox twice (one rejects.toBe,
    // one in a try/catch to inspect status+body).
    const { fetch: f } = mockFetch([
      { status: 404, body: { error: "not found" } },
      { status: 404, body: { error: "not found" } },
    ]);
    const c = new Controller({ fetch: f });
    await expect(c.getSandbox("sb-missing")).rejects.toBeInstanceOf(
      ControllerError,
    );
    try {
      await c.getSandbox("sb-missing");
      throw new Error("should have thrown");
    } catch (e) {
      expect(e).toBeInstanceOf(ControllerError);
      expect((e as ControllerError).status).toBe(404);
      expect((e as ControllerError).body).toEqual({ error: "not found" });
    }
  });

  it("authorization header set when token provided", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 200, body: [] }]);
    const c = new Controller({ token: "abc123", fetch: f });
    await c.listSnapshots();
    const headers = (calls[0]!.init as RequestInit).headers as Record<
      string,
      string
    >;
    expect(headers.authorization).toBe("Bearer abc123");
  });

  it("SANDSTORM_URL env honored as default baseUrl", async () => {
    const orig = process.env.SANDSTORM_URL;
    process.env.SANDSTORM_URL = "http://from-env";
    try {
      const c = new Controller();
      expect(c.baseUrl).toBe("http://from-env");
    } finally {
      if (orig === undefined) delete process.env.SANDSTORM_URL;
      else process.env.SANDSTORM_URL = orig;
    }
  });

  it("defaults to where a master listens", () => {
    const orig = process.env.SANDSTORM_URL;
    delete process.env.SANDSTORM_URL;
    try {
      expect(new Controller().baseUrl).toBe("https://127.0.0.1:8080");
    } finally {
      if (orig !== undefined) process.env.SANDSTORM_URL = orig;
    }
  });

  it("DELETE returns void / does not parse empty body", async () => {
    const { fetch: f } = mockFetch([{ status: 204, body: "" }]);
    const c = new Controller({ fetch: f });
    // Should not throw on empty body.
    await c.killSandbox("sb-1");
  });

  it("encodes sandbox id in URL path", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 200, body: { id: "x" } }]);
    const c = new Controller({ baseUrl: "http://x", fetch: f });
    await c.getSandbox("sb id with space");
    expect(calls[0]!.url).toBe("http://x/v1/sandboxes/sb%20id%20with%20space");
  });

  it("aborts after timeout", async () => {
    // Realistic abort path: fetch that respects AbortSignal. The
    // Controller schedules a setTimeout(timeoutMs) that calls
    // controller.abort(); a well-behaved fetch rejects with AbortError.
    const abortableFetch: typeof fetch = ((_url, init) =>
      new Promise((_, reject) => {
        const sig = (init as RequestInit | undefined)?.signal;
        if (!sig) return;
        sig.addEventListener("abort", () => {
          const err = new Error("aborted");
          err.name = "AbortError";
          reject(err);
        });
      })) as typeof fetch;
    const c = new Controller({ timeoutMs: 10, fetch: abortableFetch });
    await expect(c.listSnapshots()).rejects.toThrow(/abort/i);
  });
});
