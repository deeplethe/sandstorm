import { describe, expect, it } from "vitest";
import { backendOf, bootstrapFields, canStart, installEnv, listenCommand, pathProblem, placeBody, placeNote, placeOptions, switchBlocker, switchConsequence, templateLabel } from "./backends";
import type { ConsoleNode } from "./types";

const engine = { gvisor: "runsc version release-20261019.0", firecracker: "Firecracker v1.10.1" };
const node = (id: string, b: "gvisor" | "firecracker", over: Partial<ConsoleNode> = {}): ConsoleNode => ({
  id, pool: "default", fingerprint: "f", connected: true, last_seen_age: 0, free_mib: 4000, psi: 0, max_sandboxes: 10,
  tiers: {}, features: ["backend-switch"], snapshots: 0,
  host: { engine: engine[b], kernel: "6.8", cpu_vendor: "x", cpu_model: "y", cpu_flags: "" }, ...over,
});

describe("backendOf", () => {
  it("reads the engine's version line", () => {
    expect(backendOf(node("a", "gvisor").host)).toBe("gvisor");
    expect(backendOf(node("a", "firecracker").host)).toBe("firecracker");
    expect(backendOf(null)).toBeNull();
  });
});

describe("the install command", () => {
  const opts = { backend: "firecracker" as const, fcKernel: "/k/vmlinux", stateDir: "/data/ss" };
  it("carries the backend and paths", () => {
    expect(installEnv(opts)).toBe(" SANDSTORM_BACKEND=firecracker SANDSTORM_FC_KERNEL=/k/vmlinux SANDSTORM_STATE_DIR=/data/ss");
    expect(listenCommand("https://m", opts)).toContain("SANDSTORM_LISTEN=0.0.0.0:80 SANDSTORM_BACKEND=firecracker");
    expect(listenCommand("https://m", opts).endsWith(" sh")).toBe(true);
  });
  it("leaves the kernel out for gVisor", () => {
    const g = { ...opts, backend: "gvisor" as const };
    expect(installEnv(g)).not.toContain("FC_KERNEL");
    expect(bootstrapFields(g)).toEqual({ backend: "gvisor", state_dir: "/data/ss" });
  });
  it("holds paths to what needs no quoting", () => {
    expect(pathProblem("")).toBe("");
    expect(pathProblem("/var/lib/x_1.2")).toBe("");
    for (const bad of ["rel/x", "/a b", "/a;rm", "/a/../b", "/$(x)"]) expect(pathProblem(bad)).not.toBe("");
  });
});

describe("switching", () => {
  it("lets an empty, switchable node go", () => expect(switchBlocker(node("a", "gvisor"))).toBe(""));
  it("names what blocks it", () => {
    expect(switchBlocker(node("a", "gvisor", { tiers: { frozen: 2 } }))).toContain("2 sandboxes");
    expect(switchBlocker(node("a", "gvisor", { snapshots: 1 }))).toContain("1 snapshot");
    expect(switchBlocker(node("a", "gvisor", { connected: false }))).toContain("not answering");
    expect(switchBlocker(node("a", "gvisor", { features: [] }))).toContain("backend file");
    expect(switchBlocker(node("a", "gvisor", { switching_to: "firecracker" }))).toContain("Firecracker");
  });
  it("says the node moves group", () => {
    const s = switchConsequence(node("a", "gvisor", { pool: "default#ab" }), "firecracker");
    expect(s).toContain("host group");
    expect(s).toContain("pool default's Firecracker group");
    expect(s).toContain("rootfs.ext4");
  });
});

describe("placing", () => {
  const all = [node("g1", "gvisor", { templates: ["web"] }), node("f1", "firecracker", { pool: "default#ab", templates: ["img"] }), node("off", "gvisor", { connected: false })];
  it("offers each backend with its pools, and each node that answers", () => {
    const o = placeOptions(all);
    expect(o.map((p) => p.value)).toEqual(["", "backend:gvisor", "backend:firecracker", "node:f1", "node:g1"]);
    expect(o[2].label).toContain("Firecracker");
    expect(o[2].label).toContain("default#ab");
    expect(o[3].label).toContain("f1 · Firecracker");
  });
  it("turns a choice into the create's fields", () => {
    expect(placeBody("")).toEqual({});
    expect(placeBody("backend:firecracker")).toEqual({ backend: "firecracker" });
    expect(placeBody("node:g1")).toEqual({ node_id: "g1" });
  });
  it("warns when nothing there can start the template", () => {
    const fc = placeOptions(all).find((p) => p.value === "backend:firecracker");
    expect(placeNote(fc, "web").warn).toContain("rootfs.ext4");
    expect(placeNote(fc, "img").warn).toBe("");
    expect(placeNote(fc, "img").note).toContain("Firecracker");
    expect(placeNote(placeOptions(all)[0], "web").note).toContain("mix");
  });
  it("takes a node that has not said to have it, as the master does", () => {
    expect(canStart([node("old", "gvisor")], "web")).toBe(true);
    expect(canStart([node("n", "gvisor", { templates: [] })], "web")).toBe(false);
  });
  it("labels a template with where it is installed", () => {
    expect(templateLabel(all, "web")).toBe("web · gVisor");
    expect(templateLabel(all, "unknown")).toBe("unknown");
  });
});
