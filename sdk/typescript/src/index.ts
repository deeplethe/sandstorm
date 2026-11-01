/**
 * `@deeplethe/sandstorm` — TypeScript client for sandstorm.
 *
 * @example
 * ```ts
 * import { Controller, Sandbox } from '@deeplethe/sandstorm';
 *
 * const ctrl = new Controller({ baseUrl: 'https://127.0.0.1:8080', caFile: 'ca.pem' });
 * const templates = await ctrl.listTemplates();
 *
 * // Create + run + cleanup in one go:
 * const out = await Sandbox.with(
 *   { template: 'python' },
 *   async (sb) => sb.exec(['python3', '-c', 'print(2+2)']),
 * );
 * console.log(out.stdout); // "4\n"
 * ```
 *
 * See {@link Controller} and {@link Sandbox} for the API surface.
 */
export {
  Controller,
  ControllerError,
  SANDBOX_TOKEN_HEADER,
  type ControllerOptions,
  type CreateOptions,
} from "./controller.js";
export { Sandbox } from "./sandbox.js";
export type {
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
