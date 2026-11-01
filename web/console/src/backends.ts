// ---- backends ----------------------------------------------------------------
// Which engine a node runs its sandboxes on: gVisor (runsc), which works on
// any machine, or Firecracker, which needs KVM. One per node, chosen when it
// is installed and changeable later only while it holds nothing, because
// every sandbox and snapshot on it is in that engine's format (ADR 11). The
// engine is part of a node's fingerprint, so the two never share a group.
//
// What differs for whoever creates a sandbox is the template: runsc reads a
// rootfs directory, Firecracker an ext4 image, so a template may be installed
// for one and not the other. Nodes say which they have; the master places a
// template only where it can start, and these helpers say the same here
// before anything is sent.
//
// No imports from the page's modules, so the tests can load this alone.
import type { ConsoleNode, HostInfo } from "./types";

export type Backend = "gvisor" | "firecracker";
export type BackendChoice = "auto" | Backend;

export const BACKEND_LABEL: Record<Backend, string> = { gvisor: "gVisor", firecracker: "Firecracker" };
// The master's and the node's own name for each.
export const ENGINE: Record<Backend, "runsc" | "firecracker"> = { gvisor: "runsc", firecracker: "firecracker" };
export const fromEngine = (e: string | undefined): Backend | null => (e === "runsc" ? "gvisor" : e === "firecracker" ? "firecracker" : null);

/** Where each backend reads a template from, under the node's work dir. */
export const TEMPLATE_FORM: Record<Backend, string> = {
  gvisor: "templates/<name>/rootfs, a directory",
  firecracker: "templates/<name>/rootfs.ext4, an ext4 image",
};

/** Which backend a host's engine `--version` names. */
export function backendOf(h: HostInfo | null | undefined): Backend | null {
  const e = (h?.engine || "").trim().toLowerCase();
  return e.startsWith("runsc") ? "gvisor" : e.startsWith("firecracker") ? "firecracker" : null;
}
export const backendName = (h: HostInfo | null | undefined): string => {
  const b = backendOf(h);
  return b ? BACKEND_LABEL[b] : "unknown backend";
};

const held = (n: ConsoleNode): number => Object.values(n.tiers).reduce((a, v) => a + (v || 0), 0);

// ---- installing ----------------------------------------------------------------

/** What the Add node dialog asks the installer for. */
export interface InstallOpts { backend: BackendChoice; fcKernel: string; stateDir: string }

/** The rule the master holds a path to before it goes into a root shell:
 *  absolute, and nothing that would need quoting. */
export function pathProblem(p: string): string {
  if (!p) return "";
  if (!p.startsWith("/")) return "An absolute path, starting with /.";
  if (p.includes("..") || !/^[A-Za-z0-9._/-]+$/.test(p)) return "Letters, digits and . _ - / only, so it needs no quoting.";
  return "";
}

/** The installer variables for these options, each with a leading space.
 *  The kernel only matters where Firecracker may be picked. */
export function installEnv(o: InstallOpts): string {
  let env = ` SANDSTORM_BACKEND=${o.backend}`;
  if (o.backend !== "gvisor" && o.fcKernel) env += ` SANDSTORM_FC_KERNEL=${o.fcKernel}`;
  if (o.stateDir) env += ` SANDSTORM_STATE_DIR=${o.stateDir}`;
  return env;
}

/** The body for `POST /v1/node-bootstrap`'s backend fields. */
export function bootstrapFields(o: InstallOpts): { backend: BackendChoice; fc_kernel?: string; state_dir?: string } {
  return {
    backend: o.backend,
    ...(o.backend !== "gvisor" && o.fcKernel ? { fc_kernel: o.fcKernel } : {}),
    ...(o.stateDir ? { state_dir: o.stateDir } : {}),
  };
}

/** The line that installs a node listening for the master. */
export function listenCommand(origin: string, o: InstallOpts): string {
  return `curl -sSfL -k ${origin}/install.sh | sudo SANDSTORM_MASTER_URL=${origin} \\\n    SANDSTORM_LISTEN=0.0.0.0:80${installEnv(o)} sh`;
}

/** What the installer will do about the backend, said before it is run. */
export function installNote(b: BackendChoice): string {
  if (b === "gvisor") return "gVisor runs on any Linux machine. The installer refuses if runsc is not installed.";
  const fc = "Firecracker needs /dev/kvm, a guest kernel (an uncompressed vmlinux) and a work dir on XFS with reflink=1 or on btrfs; the installer checks each and refuses, saying which is missing, before it installs anything. It uses the firecracker on the PATH, or downloads the pinned release and checks its sha256.";
  return b === "auto" ? `Firecracker if the machine has /dev/kvm, gVisor if not; the installer prints which it picked. ${fc}` : fc;
}

// ---- switching -------------------------------------------------------------------

/** Why this node cannot change backend now, or "" if it can. */
export function switchBlocker(n: ConsoleNode): string {
  if (n.switching_to) return `It is restarting onto ${BACKEND_LABEL[fromEngine(n.switching_to) || "gvisor"]}.`;
  if (!n.connected) return "It is not answering. A switch is a command to the node, so it has to be up.";
  if (!backendOf(n.host)) return "It has not said which backend it runs yet.";
  if (!n.features.includes("backend-switch")) return "It was installed without a backend file, so its backend is fixed by its unit. Reinstall it with this master's installer to be able to switch it from here.";
  const s = held(n), t = n.snapshots || 0;
  if (s || t) {
    const what = [s ? `${s} ${s === 1 ? "sandbox" : "sandboxes"}` : "", t ? `${t} ${t === 1 ? "snapshot" : "snapshots"}` : ""].filter(Boolean).join(" and ");
    return `It holds ${what}. Each is in its backend's format and could never be restored on another, so a node switches only when it is empty: delete them first.`;
  }
  return "";
}

/** What switching to `to` will do, said before anyone confirms. */
export function switchConsequence(n: ConsoleNode, to: Backend): string {
  const from = backendOf(n.host);
  return `${n.id} will restart on ${BACKEND_LABEL[to]}${from ? ` instead of ${BACKEND_LABEL[from]}` : ""}. Its fingerprint names its engine, so it will leave its host group for pool ${n.pool.split("#")[0]}'s ${BACKEND_LABEL[to]} group, a new one if it is the first. `
    + `It needs templates as ${TEMPLATE_FORM[to]}, and the ones it has now will not start there.`
    + (to === "firecracker" ? " The node checks for /dev/kvm, its guest kernel and a work dir that can reflink first, and refuses if any is missing." : " The node checks runsc is installed first, and refuses if it is not.");
}

// ---- placing ---------------------------------------------------------------------

/** Where a create may go: anywhere, one backend's nodes, or one node. */
export interface Place { value: string; label: string; backend: Backend | null; nodes: ConsoleNode[] }

/** The choices for a create's "Where", each labelled with its backend and,
 *  for a backend, the pools its nodes are in. Only nodes that answer. */
export function placeOptions(all: ConsoleNode[]): Place[] {
  const up = all.filter((n) => n.connected).sort((a, b) => a.id.localeCompare(b.id, undefined, { numeric: true }));
  const out: Place[] = [{ value: "", label: "Any node with room", backend: null, nodes: up }];
  for (const b of ["gvisor", "firecracker"] as Backend[]) {
    const ns = up.filter((n) => backendOf(n.host) === b);
    if (!ns.length) continue;
    const pools = [...new Set(ns.map((n) => n.pool))].sort();
    out.push({ value: `backend:${b}`, label: `Any ${BACKEND_LABEL[b]} node · pool ${pools.join(", ")} (${ns.length} ${ns.length === 1 ? "node" : "nodes"})`, backend: b, nodes: ns });
  }
  for (const n of up) out.push({ value: `node:${n.id}`, label: `${n.id} · ${backendName(n.host)} · pool ${n.pool}`, backend: backendOf(n.host), nodes: [n] });
  return out;
}

/** The create body's fields for a place. */
export function placeBody(value: string): { backend?: Backend; node_id?: string } {
  if (value.startsWith("backend:")) return { backend: value.slice(8) as Backend };
  if (value.startsWith("node:")) return { node_id: value.slice(5) };
  return {};
}

/** Whether any of these nodes can start the template. A node that has not
 *  said is taken to, as the master takes it. */
export const canStart = (nodes: ConsoleNode[], tpl: string): boolean =>
  nodes.some((n) => !n.templates || n.templates.includes(tpl));

/** Which backends have this template installed somewhere, as the label's
 *  tail: "web · gVisor, Firecracker". */
export function templateLabel(all: ConsoleNode[], tpl: string): string {
  const on = (["gvisor", "firecracker"] as Backend[]).filter((b) => all.some((n) => n.connected && backendOf(n.host) === b && n.templates?.includes(tpl)));
  return on.length ? `${tpl} · ${on.map((b) => BACKEND_LABEL[b]).join(", ")}` : tpl;
}

/** What to say under the form: what the place runs on and what a template
 *  is there, and a warning when nothing there can start this one. */
export function placeNote(p: Place | undefined, tpl: string): { note: string; warn: string } {
  if (!p) return { note: "", warn: "" };
  const kinds = [...new Set(p.nodes.map((n) => backendOf(n.host)).filter((b): b is Backend => !!b))];
  const note = kinds.length === 1
    ? `Runs on ${BACKEND_LABEL[kinds[0]]}, where a template is ${TEMPLATE_FORM[kinds[0]]}.`
    : kinds.length > 1
      ? `These nodes mix gVisor and Firecracker. A template is a directory to one (${TEMPLATE_FORM.gvisor}) and an image to the other (${TEMPLATE_FORM.firecracker}); the master places it on a node that has it in its form.`
      : "";
  const warn = tpl && p.nodes.length && !canStart(p.nodes, tpl)
    ? `No node here has ${tpl} for its backend, so the master will refuse it. Install it as ${kinds.map((b) => TEMPLATE_FORM[b]).join(" or ") || "the node's backend reads it"}, or pick somewhere that has it.`
    : "";
  return { note, warn };
}
