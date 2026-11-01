import type {
  BranchOptions,
  EvalResult,
  ExecOptions,
  ExecResult,
  PingResult,
  SandboxInfo,
  SnapshotDetail,
  SnapshotInfo,
  SpawnOptions,
  Tier,
} from "./types.js";
import { fetchWithCa } from "./tls.js";

/** Where a master listens unless it was bound elsewhere. */
const DEFAULT_URL = "https://127.0.0.1:8080";

/** The master answers 504 when a node takes longer than 180 s to branch. */
const BRANCH_TIMEOUT_MS = 185_000;

/** Header carrying a sandbox's own access token (RFC 16.5 layer two). */
export const SANDBOX_TOKEN_HEADER = "x-sandstorm-sandbox-token";

/**
 * Raised on non-2xx responses from the master.
 *
 * Inspect `status` and `body` to distinguish 404 (sandbox/snapshot
 * missing) from 409 (tag collision) from 500 (internal).
 */
export class ControllerError extends Error {
  readonly status: number;
  readonly body: unknown;
  readonly url: string;

  constructor(status: number, body: unknown, url: string) {
    const snippet =
      typeof body === "string" ? body : JSON.stringify(body);
    super(`controller ${url}: HTTP ${status}: ${snippet}`);
    this.name = "ControllerError";
    this.status = status;
    this.body = body;
    this.url = url;
  }
}

export interface ControllerOptions {
  /**
   * Master base URL. Defaults to env `SANDSTORM_URL` then
   * `https://127.0.0.1:8080`, where a master listens by default.
   */
  baseUrl?: string;
  /**
   * The master's API token. Defaults to env `SANDSTORM_TOKEN`.
   */
  token?: string;
  /**
   * Per-request timeout in milliseconds. Default 60_000. Exec, eval and
   * branch wait longer when the master may legitimately take longer.
   */
  timeoutMs?: number;
  /**
   * PEM bundle to verify an `https://` master against. Defaults to env
   * `SANDSTORM_CA`. Node only.
   *
   * A v0.6 master serves TLS by default under a CA it generates itself,
   * so the public trust store cannot verify it and every call fails with
   * `fetch failed`. Point this at the master's `pki/ca.pem`, or fetch it:
   * the master serves it unauthenticated for exactly this reason.
   *
   * ```sh
   * curl -k https://127.0.0.1:8080/ca.pem -o ca.pem
   * ```
   *
   * Check it against the fingerprint in a registration token before you
   * trust it. There is deliberately no way to turn verification off: an
   * unverified control plane carries the API token and every command run
   * inside a sandbox.
   */
  caFile?: string;
  /**
   * Custom fetch implementation. Defaults to global `fetch`. Provide
   * `undici`'s fetch in Node test scenarios that want to record
   * traffic, or polyfill for older Node. It does its own TLS, so it
   * cannot be combined with `caFile`, and `SANDSTORM_CA` does not apply.
   */
  fetch?: typeof fetch;
}

/** Where and how to make a sandbox. See `POST /v1/sandboxes` in docs/API.md. */
export interface CreateOptions {
  /** Start from this template. Exactly one of this and `snapshotTag`. */
  template?: string;
  /** Fork from this tag. */
  snapshotTag?: string;
  /** Your own id. It need not be random, and it does not replace a token. */
  id?: string;
  pool?: string;
  nodeId?: string;
  /** Only on nodes running `gvisor` or `firecracker`. For a template. */
  backend?: string;
  /** The port the gateway forwards to by default. */
  port?: number;
  /** A named volume to attach. */
  volume?: string;
  /** Memory ceiling, overriding the template's. */
  memoryLimitMib?: number;
}

function createBody(options: CreateOptions): SpawnOptions {
  const body: SpawnOptions = {};
  if (options.template !== undefined) body.template = options.template;
  if (options.snapshotTag !== undefined) body.snapshot_tag = options.snapshotTag;
  if (options.id !== undefined) body.id = options.id;
  if (options.pool !== undefined) body.pool = options.pool;
  if (options.nodeId !== undefined) body.node_id = options.nodeId;
  if (options.backend !== undefined) body.backend = options.backend;
  if (options.port !== undefined) body.port = options.port;
  if (options.volume !== undefined) body.volume = options.volume;
  if (options.memoryLimitMib !== undefined) {
    body.memory_limit_mib = options.memoryLimitMib;
  }
  return body;
}

/**
 * Client for a Sandstorm master's REST API.
 *
 * @example
 * ```ts
 * import { Controller } from '@deeplethe/sandstorm';
 * const ctrl = new Controller({
 *   baseUrl: 'https://127.0.0.1:8080',
 *   token: process.env.SANDSTORM_TOKEN,
 *   caFile: process.env.SANDSTORM_CA,
 * });
 * const templates = await ctrl.listTemplates();
 * const sb = await ctrl.createSandbox({ template: 'python' });
 * // The master answers before a node has started it: see Sandbox.create.
 * const result = await ctrl.execCommand(sb.id, ['python3', '-c', 'print(2+2)'], { sandboxToken: sb.token });
 * const branch = await ctrl.branchSandbox(sb.id, { tag: 'checkpoint-1' });
 * const children = await ctrl.spawnSandboxes({ snapshotTag: branch.tag, n: 4 });
 * await ctrl.killSandbox(sb.id);
 * ```
 */
export class Controller {
  readonly baseUrl: string;
  readonly token: string | undefined;
  readonly timeoutMs: number;
  readonly caFile: string | undefined;
  private readonly fetchImpl: typeof fetch;

  constructor(opts: ControllerOptions = {}) {
    const envUrl =
      typeof process !== "undefined" ? process.env.SANDSTORM_URL : undefined;
    const envToken =
      typeof process !== "undefined" ? process.env.SANDSTORM_TOKEN : undefined;
    const envCa =
      typeof process !== "undefined" ? process.env.SANDSTORM_CA : undefined;
    // A loop, not `.replace(/\/+$/, "")`. That regex backtracks on a long
    // run of slashes followed by anything else -- O(n^2), which CodeQL
    // flags as js/polynomial-redos. The input is the caller's own base URL
    // rather than anything an attacker supplies, so the risk here is small,
    // but this is a published library and the loop is both linear and
    // easier to read.
    let base = opts.baseUrl ?? envUrl ?? DEFAULT_URL;
    while (base.endsWith("/")) base = base.slice(0, -1);
    this.baseUrl = base;
    this.token = opts.token ?? envToken ?? undefined;
    this.timeoutMs = opts.timeoutMs ?? 60_000;
    // Refused rather than ignored: the caller asked for one CA, and a custom
    // fetch would quietly verify against whatever it likes.
    if (opts.fetch && opts.caFile) {
      throw new Error(
        "Controller: `caFile` cannot be combined with `fetch`, which does its own TLS.",
      );
    }
    this.caFile = opts.caFile ?? (envCa || undefined);
    this.fetchImpl =
      opts.fetch ??
      (this.caFile && base.startsWith("https://")
        ? fetchWithCa(this.caFile)
        : globalThis.fetch);
    if (typeof this.fetchImpl !== "function") {
      throw new Error(
        "Controller: no fetch implementation. Node 18+ ships fetch globally; otherwise pass `fetch` in options.",
      );
    }
  }

  // --- templates ----------------------------------------------------

  /** The names of the registered templates. */
  async listTemplates(): Promise<string[]> {
    return this.request<string[]>("GET", "/v1/templates");
  }

  // --- snapshots ----------------------------------------------------

  async listSnapshots(): Promise<SnapshotInfo[]> {
    return this.request<SnapshotInfo[]>("GET", "/v1/snapshots");
  }

  /** One tag, plus how many sandboxes are still forked from it. */
  async getSnapshot(tag: string): Promise<SnapshotDetail> {
    return this.request<SnapshotDetail>(
      "GET",
      `/v1/snapshots/${encodeURIComponent(tag)}/info`,
    );
  }

  async deleteSnapshot(tag: string): Promise<void> {
    await this.request<null>("DELETE", `/v1/snapshots/${encodeURIComponent(tag)}`);
  }

  // --- sandboxes ----------------------------------------------------

  /**
   * Make one sandbox from a template or a tag.
   *
   * Returns the only response that carries its `token`. The master answers
   * before any node has started it, so `ip` is absent until one reports it
   * running, and {@link Sandbox.create} waits for that.
   */
  async createSandbox(options: CreateOptions): Promise<SandboxInfo> {
    return this.request<SandboxInfo>("POST", "/v1/sandboxes", createBody(options));
  }

  /**
   * Make N sandboxes from a template or a tag.
   *
   * @param options.n              At most 256. Default 1.
   * @param options.perChildNetns  Accepted and ignored: every sandbox has
   *                               its own network namespace.
   */
  async spawnSandboxes(
    options: CreateOptions & { n?: number; perChildNetns?: boolean },
  ): Promise<SandboxInfo[]> {
    const body = createBody(options);
    body.n = options.n ?? 1;
    body.per_child_netns = options.perChildNetns ?? false;
    return this.request<SandboxInfo[]>("POST", "/v1/sandboxes", body);
  }

  async listSandboxes(): Promise<SandboxInfo[]> {
    return this.request<SandboxInfo[]>("GET", "/v1/sandboxes");
  }

  async getSandbox(sandboxId: string): Promise<SandboxInfo> {
    return this.request<SandboxInfo>(
      "GET",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}`,
    );
  }

  async killSandbox(sandboxId: string): Promise<void> {
    await this.request<null>(
      "DELETE",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}`,
    );
  }

  /** Mark a sandbox used, which brings it back to hot. */
  async touchSandbox(sandboxId: string): Promise<SandboxInfo> {
    return this.request<SandboxInfo>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/touch`,
    );
  }

  /** Ask for a tier now, rather than waiting for the idle policy. */
  async setSandboxTier(sandboxId: string, tier: Tier): Promise<SandboxInfo> {
    return this.request<SandboxInfo>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/tier`,
      { tier },
    );
  }

  /** Clear a sandbox's failure so its node tries again. */
  async retrySandbox(sandboxId: string): Promise<SandboxInfo> {
    return this.request<SandboxInfo>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/retry`,
    );
  }

  /**
   * Checkpoint a running sandbox into a new tag to fork from. The parent
   * keeps running, after a short pause, and this returns once the node has
   * written the image.
   *
   * The sandbox must be hot: the master answers 409 otherwise, and
   * {@link touchSandbox} wakes it. Returns a {@link SnapshotInfo}; pass its
   * `tag` to {@link spawnSandboxes} to fan out children.
   */
  async branchSandbox(
    sandboxId: string,
    options: BranchOptions,
  ): Promise<SnapshotInfo> {
    return this.request<SnapshotInfo>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/branch`,
      { tag: options.tag },
      undefined,
      Math.max(this.timeoutMs, BRANCH_TIMEOUT_MS),
    );
  }

  /**
   * Run a subprocess in the sandbox.
   *
   * `sandboxToken` is the `token` field of the response that created the
   * sandbox. From v0.6 the master requires it on every call that reaches
   * into a sandbox: the bearer token says you may talk to the API, this
   * says you may enter *this* sandbox. Omitting it gets a 403.
   */
  async execCommand(
    sandboxId: string,
    args: string[],
    options: { timeoutSecs?: number; sandboxToken?: string } = {},
  ): Promise<ExecResult> {
    const timeoutSecs = options.timeoutSecs ?? 30;
    const body: ExecOptions = { args, timeout_secs: timeoutSecs };
    return this.request<ExecResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/exec`,
      body,
      options.sandboxToken,
      this.commandTimeoutMs(timeoutSecs),
    );
  }

  /**
   * Run a snippet with the template's `eval_argv`, `python3 -c` by default.
   * `result` is what it printed. See {@link execCommand} for `sandboxToken`.
   */
  async evalCode(
    sandboxId: string,
    code: string,
    sandboxToken?: string,
    timeoutSecs = 30,
  ): Promise<EvalResult> {
    return this.request<EvalResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/eval`,
      { code, timeout_secs: timeoutSecs },
      sandboxToken,
      this.commandTimeoutMs(timeoutSecs),
    );
  }

  /** Check the sandbox answers. See {@link execCommand} for `sandboxToken`. */
  async pingSandbox(
    sandboxId: string,
    sandboxToken?: string,
  ): Promise<PingResult> {
    return this.request<PingResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/ping`,
      undefined,
      sandboxToken,
    );
  }

  // --- internals ----------------------------------------------------

  /**
   * How long to wait for a command the sandbox may run for this long. The
   * master gives the node `timeoutSecs` plus 20 s and then answers 504, so
   * waiting a little longer gets its answer rather than our own.
   */
  private commandTimeoutMs(timeoutSecs: number): number {
    return Math.max(this.timeoutMs, (timeoutSecs + 25) * 1000);
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    sandboxToken?: string,
    timeoutMs: number = this.timeoutMs,
  ): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;
    // A header, not the path or query: this is a secret, and a query string
    // ends up in logs and in Referer.
    if (sandboxToken) headers[SANDBOX_TOKEN_HEADER] = sandboxToken;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const resp = await this.fetchImpl(url, {
        method,
        headers,
        body: body !== undefined ? JSON.stringify(body) : undefined,
        signal: controller.signal,
      });
      if (!resp.ok) {
        let parsed: unknown;
        const text = await resp.text();
        try {
          parsed = text ? JSON.parse(text) : {};
        } catch {
          parsed = text;
        }
        throw new ControllerError(resp.status, parsed, url);
      }
      // DELETE returns 204 / empty; tolerate.
      const text = await resp.text();
      if (!text) return null as T;
      return JSON.parse(text) as T;
    } finally {
      clearTimeout(timer);
    }
  }
}
