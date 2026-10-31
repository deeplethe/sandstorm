// The shapes of what the master's API answers, as far as the console reads
// them. The Rust side is the authority: `master/api.rs` (ConsoleNow,
// ConsoleNode, SeriesView, SandboxView, SnapshotView, ExecView,
// BootstrapView, AddedNode), `master/series.rs` (Line, Event), `state.rs`
// (PoolStretch) and `proto.rs` (HostInfo).

export type Tier = "hot" | "frozen" | "snapshot" | "detached";

/** `[unix seconds, value]`, oldest first. */
export type Point = [number, number];

/** What a node's fingerprint is made of (`proto::HostInfo`). */
export interface HostInfo {
  /** The engine's `--version`, e.g. `runsc version release-20261019.0`. */
  engine: string;
  /** `/proc/sys/kernel/osrelease`. */
  kernel: string;
  cpu_vendor: string;
  cpu_model: string;
  /** The `flags` line of `/proc/cpuinfo`, space separated. */
  cpu_flags: string;
}

/** Counts by tier. The master only sends the tiers that have any. */
export type TierCounts = Partial<Record<Tier, number>>;

export interface ConsoleNode {
  id: string;
  pool: string;
  /** The pool's own hash; a node in a derived pool `<asked>#<fp8>` landed
   *  there because this differs from the rest of `<asked>`. */
  fingerprint: string;
  /** Absent until the node has reported what its host is. */
  host?: HostInfo;
  connected: boolean;
  last_seen_age: number;
  free_mib: number;
  psi: number;
  max_sandboxes: number;
  tiers: TierCounts;
  features: string[];
  /** Snapshot tags baked on it, which like its sandboxes keep it from
   *  changing backend. Absent from older masters. */
  snapshots?: number;
  /** Templates it can start on its backend, once it has said. */
  templates?: string[];
  /** The backend it is restarting onto, while it does. */
  switching_to?: "runsc" | "firecracker";
}

/** Which pool a node was in, and when (`state::PoolStretch`). */
export interface PoolStretch {
  node: string;
  pool: string;
  since: number;
  until: number | null;
}

export interface ConsoleFailing {
  id: string;
  node: string;
  error: string;
  attempts: number;
}

/** `GET /v1/console/now`: everything the console draws that is a single
 *  "now" rather than a line. */
export interface ConsoleNow {
  now: number;
  pool_history: PoolStretch[];
  uptime_secs: number;
  /** Idle seconds before frozen, on disk and detached. */
  idle: [number, number, number];
  tiers: TierCounts;
  failed: number;
  snapshots: number;
  snapshot_bytes: number;
  templates: string[];
  nodes: ConsoleNode[];
  failing: ConsoleFailing[];
  counters: Record<string, number>;
  /** The admission thresholds a node is held to. */
  floor_mib: number;
  max_psi: number;
}

/** One line of history (`series::Line`). */
export interface SeriesLine {
  metric: string;
  key: string;
  points: Point[];
}

/** `GET /v1/console/series`. */
export interface SeriesView {
  now: number;
  since: number;
  step: number;
  every: number;
  lines: SeriesLine[];
}

/** One row of `GET /v1/console/events` (`series::Event`). */
export interface ConsoleEvent {
  ts: number;
  kind: string;
  subject: string;
  detail: string;
}

/** A sandbox as `GET /v1/sandboxes` serves it. */
/** What a sandbox may reach; see `Egress` in proto.rs. */
export interface Egress {
  internet: boolean;
  allow?: string[];
  deny?: string[];
}

export interface SandboxView {
  id: string;
  node_id: string;
  pool: string;
  template: string;
  tier: Tier;
  snapshot_tag?: string;
  guest_addr?: string;
  created_at_unix: number;
  netns?: string;
  memory_limit_mib?: number;
  egress: Egress;
  volume: string | null;
  port: number | null;
  gateway_port?: number;
  ip?: string;
  created_at: number;
  last_active_at: number;
  last_error?: string;
  error_attempts?: number;
}

/** What `POST /v1/sandboxes` answers: the view, with the token it shows
 *  once. A batch create answers an array of them. */
export interface CreatedSandbox extends SandboxView {
  token?: string;
}

/** A snapshot as `GET /v1/snapshots` serves it. */
export interface SnapshotView {
  tag: string;
  node_id: string;
  template: string;
  branched_from?: string;
  created_at_unix: number;
  dir: string;
  from_sandbox?: string;
  created_at: number;
  size_bytes: number;
}

/** `GET /v1/templates`: names; older masters sent objects with one. */
/** `GET /v1/templates/pulls`: a template from an image, node by node. */
export interface PullView {
  name: string;
  image: string;
  ready: boolean;
  nodes: { node_id: string; state: "pulling" | "ready" | "failed"; error?: string }[];
}

export type TemplateEntry = string | { name: string };

/** `POST /v1/sandboxes/{id}/exec`. */
export interface ExecView {
  stdout: string;
  stderr: string;
  exit_code: number;
}

/** `POST /v1/node-bootstrap`. */
export interface BootstrapView {
  token: string;
  pool: string;
  expires_at: number;
  install_command: string;
  ca_fingerprint?: string;
  first_fetch_is_unverified: boolean;
}

/** `POST /v1/nodes/{id}/backend`. */
export interface BackendSwitchView {
  node: string;
  from: "runsc" | "firecracker" | null;
  to: "runsc" | "firecracker";
  restarting: boolean;
  message: string;
}

/** `POST /v1/nodes`. */
export interface AddedNode {
  id: string;
  pool: string;
  address: string;
  fingerprint: string;
  max_sandboxes: number;
}
