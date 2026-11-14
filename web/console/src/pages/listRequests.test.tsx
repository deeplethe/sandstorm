// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setToken } from "../api";
import { emit, setTab, st } from "../state";
import { Activity } from "./Activity";
import { Sandboxes } from "./Sandboxes";

const pages = [
  {
    tab: "sandboxes",
    Component: Sandboxes,
    endpoint: "/v1/sandboxes",
    filter: 'button[data-t="frozen"]',
    filterKey: "tier",
    filterValue: "frozen",
    row: (id: string, filtered: boolean) => ({
      id, tier: filtered ? "frozen" : "hot", egress: { internet: true }, last_active_at: 0,
    }),
  },
  {
    tab: "activity",
    Component: Activity,
    endpoint: "/v1/console/events",
    filter: 'button[data-k="node_down,node_up"]',
    filterKey: "kind",
    filterValue: "node_down,node_up",
    row: (subject: string, filtered: boolean) => ({
      ts: 1, kind: filtered ? "node_down" : "to_disk", subject, detail: "",
    }),
  },
];

let container: HTMLElement;
let root: Root;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  setToken("test-master-token");
  st.refreshed = 0;
  st.gate = { ...st.gate, open: false };
  container = document.body.appendChild(document.createElement("div"));
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  setToken(null);
  window.localStorage.clear();
  vi.unstubAllGlobals();
});

function mockMaster(endpoint: string) {
  const requests: Array<{ query: URLSearchParams; respond: (response: Response) => void }> = [];
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    const url = new URL(path, "http://test");
    if (url.pathname === "/v1/egress") return Promise.resolve(Response.json({ internet: true }));
    if (url.pathname !== endpoint) throw new Error(`Unexpected request: ${path}`);
    return new Promise<Response>((respond) => { requests.push({ query: url.searchParams, respond }); });
  }));
  return requests;
}

const list = (row: unknown, total: number) =>
  Response.json([row], { headers: { "x-total-count": String(total) } });

async function click(selector: string) {
  const button = container.querySelector<HTMLButtonElement>(selector);
  expect(button, selector).not.toBeNull();
  await act(async () => { button!.click(); });
}

function tableRows() {
  return container.querySelector("tbody")?.textContent ?? "";
}

describe.each(pages)("$tab list requests", ({ tab, Component, endpoint, filter, filterKey, filterValue, row }) => {
  it("replaces a pending unfiltered request with the selected filter", async () => {
    const requests = mockMaster(endpoint);
    setTab(tab);
    await act(async () => { root.render(<Component />); });
    const unfiltered = requests[0];
    expect(unfiltered.query.get(filterKey)).toBeNull();

    await click(filter);
    expect(container.querySelector(filter)?.getAttribute("aria-pressed")).toBe("true");
    const inFlight = requests.find(({ query }) => query.get(filterKey) === filterValue);
    if (inFlight) {
      await act(async () => { inFlight.respond(list(row("filtered-result", true), 1)); });
    }
    await act(async () => { unfiltered.respond(list(row("unfiltered-stale", false), 1)); });
    expect(tableRows()).not.toContain("unfiltered-stale");

    const filtered = requests.find(({ query }) => query.get(filterKey) === filterValue);
    expect(filtered?.query.get("offset")).toBe("0");
    if (!inFlight) {
      await act(async () => { filtered!.respond(list(row("filtered-result", true), 1)); });
    }
    expect(tableRows()).toContain("filtered-result");
  });

  it("renders the latest page when a previous page response arrives after navigation", async () => {
    const requests = mockMaster(endpoint);
    setTab(tab);
    await act(async () => { root.render(<Component />); });
    expect(requests[0].query.get("offset")).toBe("0");
    await act(async () => { requests[0].respond(list(row("first-result", false), 61)); });
    expect(tableRows()).toContain("first-result");

    await click('button[aria-label="Page 2"]');
    const second = requests.find(({ query }) => query.get("offset") === "20");
    expect(second).toBeDefined();
    await click('button[aria-label="Page 3"]');
    const inFlight = requests.find(({ query }) => query.get("offset") === "40");
    if (inFlight) {
      await act(async () => { inFlight.respond(list(row("third-result", false), 61)); });
    }
    await act(async () => { second!.respond(list(row("second-stale", false), 61)); });
    expect(tableRows()).not.toContain("second-stale");

    const third = requests.find(({ query }) => query.get("offset") === "40");
    expect(third).toBeDefined();
    if (!inFlight) {
      await act(async () => { third!.respond(list(row("third-result", false), 61)); });
    }
    expect(tableRows()).toContain("third-result");
    expect(container.querySelector('button[aria-current="page"]')?.getAttribute("aria-label")).toBe("Page 3");
  });

  it("loads a filtered first page after discarding an earlier page error", async () => {
    const requests = mockMaster(endpoint);
    setTab(tab);
    await act(async () => { root.render(<Component />); });
    await act(async () => { requests[0].respond(list(row("first-result", false), 61)); });

    await click('button[aria-label="Page 2"]');
    const previous = requests.find(({ query }) => query.get("offset") === "20");
    expect(previous).toBeDefined();
    await click(filter);
    expect(container.querySelector(filter)?.getAttribute("aria-pressed")).toBe("true");

    await act(async () => { previous!.respond(new Response("obsolete page error", { status: 503 })); });
    expect(container.textContent).not.toContain("obsolete page error");
    const filtered = requests.find(({ query }) => query.get(filterKey) === filterValue);
    expect(filtered?.query.get("offset")).toBe("0");

    await act(async () => { filtered!.respond(list(row("filtered-first-page", true), 1)); });
    expect(tableRows()).toContain("filtered-first-page");
    expect(container.querySelector('button[aria-current="page"]')?.getAttribute("aria-label")).toBe("Page 1");
  });

  it("ignores a late unauthorized result for a superseded query", async () => {
    const requests = mockMaster(endpoint);
    setTab(tab);
    await act(async () => { root.render(<Component />); });
    await click(filter);

    await act(async () => { requests[0].respond(new Response(null, { status: 401 })); });
    expect(st.gate.open).toBe(false);
  });

  it("can publish a slow result while same-query background refreshes arrive", async () => {
    const requests = mockMaster(endpoint);
    setTab(tab);
    await act(async () => { root.render(<Component />); });
    const initial = requests[0];
    for (let i = 0; i < 2; i++) {
      await act(async () => { st.refreshed++; emit(); });
    }
    expect(requests).toHaveLength(1);

    await act(async () => { initial.respond(list(row("slow-result", false), 1)); });
    expect(tableRows()).toContain("slow-result");
    expect(requests.length).toBeLessThanOrEqual(2);
  });
});
