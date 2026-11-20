import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { loading as renderLoading } from "./loading";

vi.mock("./loading", () => ({ loading: vi.fn() }));

let state: typeof import("./state");
const loading = vi.mocked(renderLoading);
let requests: Array<{ path: string; resolve: (response: Response) => void }>;

beforeEach(async () => {
  vi.resetModules();
  vi.useFakeTimers();
  vi.setSystemTime(10_000);
  vi.stubGlobal("location", { hash: "" });
  vi.stubGlobal("window", globalThis);
  for (const storage of ["localStorage", "sessionStorage"]) {
    vi.stubGlobal(storage, { getItem: vi.fn(() => null), setItem: vi.fn(), removeItem: vi.fn() });
  }
  requests = [];
  vi.stubGlobal("fetch", vi.fn((path: string) => new Promise<Response>((resolve) => {
    requests.push({ path, resolve });
  })));
  state = await import("./state");
  loading.mockClear();
  (await import("./api")).setToken("test-token");
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function reply(index: number, sample: number, status = 200) {
  const now = requests[index * 2];
  const series = requests[index * 2 + 1];
  expect(now.path).toBe("/v1/console/now");
  expect(series.path).toContain("/v1/console/series?since=");
  now.resolve(Response.json(status === 200 ? { now: sample, nodes: [] } : { error: "master unavailable" }, { status }));
  series.resolve(Response.json({ now: sample }));
}

function expectSample(sample: number) {
  expect(state.st.NOW?.now).toBe(sample);
  expect(state.st.S?.now).toBe(sample);
}

describe("fleet refresh ordering", () => {
  it("keeps the newer result when two reads of the same range finish out of order", async () => {
    const older = state.refresh();
    const newer = state.refresh();
    reply(1, 2);
    await newer;
    const lastOk = state.st.lastOk;
    const refreshed = state.st.refreshed;

    vi.setSystemTime(15_000);
    reply(0, 1);
    await older;

    expectSample(2);
    expect(state.st.lastOk).toBe(lastOk);
    expect(state.st.refreshed).toBe(refreshed);
  });

  it("invalidates the original request on an A → B → A range change before the new response", async () => {
    const original = state.refresh();
    state.setRange(3600);
    state.setRange(900);
    expect([requests[1].path, requests[3].path, requests[5].path]).toEqual([
      "/v1/console/series?since=900&step=5",
      "/v1/console/series?since=3600",
      "/v1/console/series?since=900&step=5",
    ]);

    reply(0, 1);
    await original;

    expect(state.st.NOW).toBeNull();
    expect(state.st.S).toBeNull();
    reply(1, 2);
    reply(2, 3);
    await vi.advanceTimersByTimeAsync(0);
    expectSample(3);
  });

  it("publishes slow results even while newer polls are still in flight", async () => {
    const first = state.refresh();
    await vi.advanceTimersByTimeAsync(5000);
    const second = state.refresh();
    await vi.advanceTimersByTimeAsync(2000);
    reply(0, 1);
    await first;
    expectSample(1);

    await vi.advanceTimersByTimeAsync(3000);
    const third = state.refresh();
    await vi.advanceTimersByTimeAsync(2000);
    reply(1, 2);
    await second;
    expectSample(2);

    reply(2, 3);
    await third;
    expectSample(3);
  });

  it.each([503, 401])("ignores an older HTTP %i after a newer poll succeeds", async (status) => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
    const older = state.refresh();
    const newer = state.refresh();
    reply(1, 2);
    await newer;
    loading.mockClear();

    reply(0, 1, status);
    await older;

    expectSample(2);
    expect(state.st.liveErr).toBeNull();
    expect(state.st.gate.open).toBe(false);
    expect(loading).not.toHaveBeenCalled();
    expect(warning).not.toHaveBeenCalled();
  });

  it("ignores the previous token's refusal while a new sign-in is pending", async () => {
    const previous = state.refresh();
    state.signOut();
    expect(state.st.gate.open).toBe(true);
    state.signIn("replacement-token");

    reply(0, 1, 401);
    await previous;

    expect(state.st.gate.open).toBe(false);
    expect(state.st.NOW).toBeNull();
    reply(1, 2);
    await vi.advanceTimersByTimeAsync(0);
    expectSample(2);
    expect(state.st.gate.open).toBe(false);
  });

  it("displays the login gate when the current refresh returns HTTP 401", async () => {
    const current = state.refresh();
    reply(0, 1, 401);
    await current;

    expect(state.st.gate).toMatchObject({ open: true, msg: "That token was refused." });
  });

  it("does not reopen the delayed range loader after a newer poll succeeds", async () => {
    const initial = state.refresh(true);
    reply(0, 1);
    await initial;
    loading.mockClear();

    state.setRange(3600);
    const poll = state.refresh();
    reply(2, 3);
    await poll;
    await vi.advanceTimersByTimeAsync(250);

    expectSample(3);
    expect(loading).not.toHaveBeenCalledWith(true, "Loading the last 1h");
    reply(1, 2);
    await vi.advanceTimersByTimeAsync(0);
  });

  it("closes the range loader on failure if it can keep displaying previous data", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    const initial = state.refresh(true);
    reply(0, 1);
    await initial;
    loading.mockClear();

    state.setRange(3600);
    await vi.advanceTimersByTimeAsync(250);
    expect(loading).toHaveBeenLastCalledWith(true, "Loading the last 1h");

    reply(1, 2, 503);
    await vi.advanceTimersByTimeAsync(0);

    expectSample(1);
    expect(state.st.liveErr).toMatchObject({ status: 503 });
    expect(loading).toHaveBeenLastCalledWith(false);
  });
});
