/**
 * Wire-level types for a Sandstorm master's REST API.
 *
 * Source of truth: `docs/API.md`.
 */

/** Where a sandbox is in the idle lifecycle, warmest first. */
export type Tier = "hot" | "frozen" | "snapshot" | "detached";

export interface SnapshotInfo {
  tag: string;
  node_id: string;
  template: string;
  /** The sandbox it was branched from. */
  from_sandbox?: string;
  /** The v0.5 name for `from_sandbox`. */
  branched_from?: string;
  dir: string;
  size_bytes: number;
  created_at: number;
  /** The v0.5 name for `created_at`. */
  created_at_unix: number;
}

/** {@link SnapshotInfo} from `GET /v1/snapshots/:tag/info`. */
export interface SnapshotDetail extends SnapshotInfo {
  /** How many sandboxes are still forked from this tag. */
  children: number;
}

export interface SandboxInfo {
  id: string;
  /** Only on the response that created it, so keep it. */
  token?: string;
  node_id: string;
  pool: string;
  template: string;
  tier: Tier;
  port: number | null;
  /** Absent when none was free: it is then reachable only from its node. */
  gateway_port?: number;
  /** Absent until a node reports the sandbox running. */
  ip?: string;
  /** Absent until a node reports the sandbox running. */
  guest_addr?: string;
  netns?: string;
  volume: string | null;
  created_at: number;
  /** The v0.5 name for `created_at`. */
  created_at_unix: number;
  last_active_at: number;
  /** The tag it was forked from, when it was. */
  snapshot_tag?: string;
  /** The override in force. Absent when the template's applies. */
  memory_limit_mib?: number;
  /** Why it is not up, when it is not. */
  last_error?: string;
  error_attempts?: number;
}

/** The body of `POST /v1/sandboxes`. */
export interface SpawnOptions {
  /** Start from this template. Exactly one of this and `snapshot_tag`. */
  template?: string;
  /** Fork from this tag. */
  snapshot_tag?: string;
  /** How many to make, at most 256. Also makes the answer a list. */
  n?: number;
  id?: string;
  pool?: string;
  node_id?: string;
  /** Only on nodes running `gvisor` or `firecracker`. For a template. */
  backend?: string;
  port?: number;
  volume?: string;
  memory_limit_mib?: number;
  /** Accepted and ignored: every sandbox has its own namespace. */
  per_child_netns?: boolean;
}

export interface BranchOptions {
  /** Name for the new tag: no slashes, no leading dots. */
  tag: string;
}

export interface ExecOptions {
  args: string[];
  timeout_secs?: number;
}

export interface ExecResult {
  stdout: string;
  stderr: string;
  exit_code: number;
}

/** A result or an error, never both. */
export interface EvalResult {
  /** What the snippet printed. */
  result?: string;
  error?: string;
  exit_code: number;
}

export interface PingResult {
  /** Whether something accepts connections on the sandbox's port. */
  ok: boolean;
  tier: Tier;
  guest_addr?: string;
  latency_ms: number;
}
