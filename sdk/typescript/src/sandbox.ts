import {
  Controller,
  type ControllerOptions,
  type CreateOptions,
} from "./controller.js";
import type { ExecResult, PingResult, SandboxInfo, SnapshotInfo } from "./types.js";

/**
 * The master stops retrying a sandbox that has failed to start this many
 * times (`MAX_FAILURES` in `master/api.rs`), so waiting longer is futile.
 */
const MAX_START_FAILURES = 5;

/**
 * Higher-level wrapper around a single sandbox. Mirrors the shape of
 * E2B / Daytona SDKs so existing agent code can swap in sandstorm with
 * minimal changes.
 *
 * Lifecycle:
 *
 * ```ts
 * import { Sandbox } from '@deeplethe/sandstorm';
 *
 * // Either create + own:
 * const sb = await Sandbox.create({ template: 'python' });
 * try {
 *   const r = await sb.exec(['python3', '-c', 'print(2+2)']);
 *   console.log(r.stdout); // "4\n"
 * } finally {
 *   await sb.kill();
 * }
 *
 * // Or wrap one you made, from the response that created it (e.g., after
 * // spawning N via Controller.spawnSandboxes):
 * const sb2 = new Sandbox(controller, info);
 * await sb2.waitUntilRunning();
 * ```
 */
export class Sandbox {
  readonly id: string;
  info: SandboxInfo;
  private readonly controller: Controller;
  // Kept apart from `info`, which later reads replace: only the create
  // response carries it, and nothing can reach into the sandbox without it.
  private readonly token: string | undefined;
  private killed = false;

  constructor(controller: Controller, info: SandboxInfo) {
    this.controller = controller;
    this.info = info;
    this.id = info.id;
    this.token = info.token;
  }

  /**
   * Create one sandbox and wait until it is running. The most common entry
   * point. Give a `template` to start from, or a `snapshotTag` to fork from.
   *
   * @example
   * ```ts
   * const sb = await Sandbox.create({ template: 'python' });
   * const child = await Sandbox.create({ snapshotTag: 'checkpoint-1' });
   * ```
   */
  static async create(
    options: CreateOptions & { startTimeoutMs?: number } & ControllerOptions,
  ): Promise<Sandbox> {
    const {
      baseUrl,
      token,
      timeoutMs,
      caFile,
      fetch: fetchImpl,
      startTimeoutMs,
      ...createOptions
    } = options;
    if ((createOptions.template === undefined) === (createOptions.snapshotTag === undefined)) {
      throw new Error("Sandbox.create: give a template or a snapshotTag, not both or neither");
    }
    const ctrl = new Controller({ baseUrl, token, timeoutMs, caFile, fetch: fetchImpl });
    const sb = new Sandbox(ctrl, await ctrl.createSandbox(createOptions));
    try {
      await sb.waitUntilRunning(startTimeoutMs);
    } catch (e) {
      // Remove what never came up, keeping the original error.
      await sb.kill().catch(() => undefined);
      throw e;
    }
    return sb;
  }

  /**
   * Wait until a node reports this sandbox running.
   *
   * The master answers a create before any node has acted on it, and an
   * exec that arrives first fails. A node reports the address only for a
   * sandbox with processes, so that is the signal. `ping` is not: its `ok`
   * also needs something listening inside.
   *
   * A program that fails to load never gets that far: the master quarantines
   * it, which throws here.
   */
  async waitUntilRunning(startTimeoutMs = 60_000): Promise<void> {
    const deadline = Date.now() + startTimeoutMs;
    let pauseMs = 50;
    for (;;) {
      const seen = await this.controller.getSandbox(this.id);
      if (seen.ip) {
        this.info = { ...this.info, ...seen };
        return;
      }
      const reason = seen.last_error ?? "no reason recorded";
      if ((seen.error_attempts ?? 0) >= MAX_START_FAILURES) {
        throw new Error(`sandbox ${this.id} failed to start: ${reason}`);
      }
      if (Date.now() + pauseMs > deadline) {
        throw new Error(
          `sandbox ${this.id} did not start within ${startTimeoutMs} ms; last error: ${reason}`,
        );
      }
      await new Promise((done) => setTimeout(done, pauseMs));
      pauseMs = Math.min(pauseMs * 2, 1000);
    }
  }

  /** Run a subprocess in the sandbox. */
  async exec(
    args: string[],
    options: { timeoutSecs?: number } = {},
  ): Promise<ExecResult> {
    return this.controller.execCommand(this.id, args, {
      ...options,
      sandboxToken: this.token,
    });
  }

  /**
   * Run a snippet and return what it printed, trailing newline included.
   *
   * The master appends `code` to the template's `eval_argv`, `python3 -c` by
   * default, so each call is a fresh process and only printed output comes
   * back: use `print(2+2)`, not a bare expression. Throws when it fails.
   */
  async eval(
    code: string,
    options: { timeoutSecs?: number } = {},
  ): Promise<string> {
    const r = await this.controller.evalCode(
      this.id,
      code,
      this.token,
      options.timeoutSecs,
    );
    if (r.error !== undefined) {
      throw new Error(`eval error (exit ${r.exit_code}): ${r.error}`);
    }
    return r.result ?? "";
  }

  /** Check the sandbox answers. See {@link PingResult}. */
  async ping(): Promise<PingResult> {
    return this.controller.pingSandbox(this.id, this.token);
  }

  /** Mark it used, which brings it back to hot. */
  async touch(): Promise<void> {
    this.info = { ...this.info, ...(await this.controller.touchSandbox(this.id)) };
  }

  /**
   * Checkpoint this sandbox into a new tag. Returns the snapshot info; pass
   * its `tag` to `Sandbox.create` or `Controller.spawnSandboxes` to fan out
   * children that inherit this sandbox's exact state.
   *
   * It must be hot: the master answers 409 otherwise, and {@link touch}
   * wakes it.
   */
  async branch(options: { tag: string }): Promise<SnapshotInfo> {
    return this.controller.branchSandbox(this.id, options);
  }

  /** Terminate the sandbox. Idempotent. */
  async kill(): Promise<void> {
    if (this.killed) return;
    await this.controller.killSandbox(this.id);
    this.killed = true;
  }

  /** Convenience: create + run callback + kill. */
  static async with<T>(
    options: Parameters<typeof Sandbox.create>[0],
    fn: (sb: Sandbox) => Promise<T>,
  ): Promise<T> {
    const sb = await Sandbox.create(options);
    try {
      return await fn(sb);
    } finally {
      await sb.kill();
    }
  }
}
