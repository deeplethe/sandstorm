// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setToken } from "../api";
import { BuildSnapshot } from "./BuildSnapshot";

const buildId = "build-a";
const sandboxPath = "/v1/sandboxes/" + buildId;
const snapshot = { tag: "python-v1", node_id: "node-a" };
let root: Root | undefined;
let container: HTMLElement | undefined;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  setToken("test-master-token");
  window.sessionStorage.setItem("sandstorm-build", JSON.stringify({
    id: buildId, token: "test-build-token", node: "node-a", cwd: "/root",
  }));
});

afterEach(async () => {
  await unmountBuilder();
  window.sessionStorage.clear();
  setToken(null);
  vi.unstubAllGlobals();
});

function mockMaster() {
  const requests: string[] = [];
  let deletes = 0;
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const request = `${init?.method ?? "GET"} ${path}`;
    requests.push(request);
    switch (request) {
      case "GET /v1/templates":
        return Response.json([]);
      case `GET ${sandboxPath}`:
        return Response.json({ id: buildId });
      case `POST ${sandboxPath}/exec`:
        return Response.json({ exit_code: 0, stdout: "/root" });
      case `POST ${sandboxPath}/branch`:
        return Response.json(snapshot);
      case `DELETE ${sandboxPath}`:
        if (deletes++ === 0) return Response.json({ error: "cleanup unavailable" }, { status: 503 });
        return new Response(null, { status: 204 });
      default:
        throw new Error(`Unexpected request: ${request}`);
    }
  }));
  return requests;
}

async function mountBuilder(onSaved = vi.fn()) {
  container = document.body.appendChild(document.createElement("div"));
  root = createRoot(container);
  await act(async () => {
    root!.render(<BuildSnapshot open onClose={() => {}} onLive={() => {}} onSaved={onSaved} />);
  });
  return onSaved;
}

async function unmountBuilder() {
  if (!root) return;
  await act(async () => { root!.unmount(); });
  container!.remove();
  root = undefined;
  container = undefined;
}

function visibleButton(name: string) {
  return Array.from(document.querySelectorAll<HTMLButtonElement>("button")).find(
    (button) => button.textContent?.trim() === name && !button.closest("[hidden]"),
  );
}

async function click(name: string) {
  const button = visibleButton(name);
  expect(button, `visible button "${name}"`).toBeDefined();
  await act(async () => { button!.click(); });
}

async function saveSnapshot() {
  const name = document.querySelector<HTMLInputElement>('[aria-label="Snapshot name"]')!;
  name.value = snapshot.tag;
  await click("Save snapshot");
}

function terminal() {
  return document.querySelector('[role="log"]')?.textContent;
}

function rememberedBuild() {
  const raw = window.sessionStorage.getItem("sandstorm-build");
  return raw ? JSON.parse(raw) : null;
}

describe("snapshot builder cleanup", () => {
  it("keeps a saved snapshot and retries only deletion after HTTP 503", async () => {
    const requests = mockMaster();
    const onSaved = await mountBuilder();
    await saveSnapshot();

    expect(onSaved).toHaveBeenCalledOnce();
    expect(terminal()).toContain(`Saved ${snapshot.tag}`);
    expect(terminal()).toContain("cleanup unavailable");
    expect(terminal()).not.toContain(`Deleted the build sandbox ${buildId}`);
    expect(rememberedBuild()).toMatchObject({ id: buildId });
    expect(visibleButton("Save snapshot")).toBeUndefined();
    expect(visibleButton("Fork a sandbox from it")).toBeDefined();

    await click("Retry delete sandbox");

    expect(terminal()).toContain(`Deleted the build sandbox ${buildId}`);
    expect(rememberedBuild()).toBeNull();
    expect(onSaved).toHaveBeenCalledOnce();
    expect(requests.filter((request) => request.endsWith("/branch"))).toEqual([`POST ${sandboxPath}/branch`]);
    expect(requests.filter((request) => request.startsWith("DELETE "))).toEqual([
      `DELETE ${sandboxPath}`, `DELETE ${sandboxPath}`,
    ]);
  });

  it("resumes pending deletion after a reload without saving the snapshot again", async () => {
    const requests = mockMaster();
    await mountBuilder();
    await saveSnapshot();
    await unmountBuilder();

    const onSaved = await mountBuilder();
    expect(rememberedBuild()).toMatchObject({ id: buildId });
    expect(visibleButton("Save snapshot")).toBeUndefined();
    expect(visibleButton("Fork a sandbox from it")).toBeDefined();
    await click("Retry delete sandbox");

    expect(rememberedBuild()).toBeNull();
    expect(onSaved).not.toHaveBeenCalled();
    expect(requests.filter((request) => request.endsWith("/branch"))).toEqual([`POST ${sandboxPath}/branch`]);
  });

  it("keeps a discardable build after HTTP 503 and clears it after HTTP 204", async () => {
    const requests = mockMaster();
    await mountBuilder();
    await click("Discard");
    await click("Click again to discard");

    expect(terminal()).toContain("cleanup unavailable");
    expect(rememberedBuild()).toMatchObject({ id: buildId });
    expect(visibleButton("Save snapshot")).toBeDefined();

    await click("Discard");
    await click("Click again to discard");

    expect(rememberedBuild()).toBeNull();
    expect(requests.filter((request) => request.startsWith("DELETE "))).toEqual([
      `DELETE ${sandboxPath}`, `DELETE ${sandboxPath}`,
    ]);
  });
});
