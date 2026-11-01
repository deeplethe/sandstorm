import { session } from "./storage";

// The API token, kept in this tab only: sessionStorage is gone when the tab
// closes, which is what the gate promises.
export const auth: { token: string | null; refused: () => void } = {
  token: session.get("sandstorm-token"),
  // Set by the state module: a 401 brings the gate back.
  refused: () => {},
};

export function setToken(t: string | null): void {
  auth.token = t;
  if (t == null) session.remove("sandstorm-token");
  else session.set("sandstorm-token", t);
}

/** An error the API answered, with its status when there was one. */
export class ApiError extends Error {
  status?: number;
  constructor(message: string, status?: number) {
    super(message);
    this.status = status;
  }
}

const bearer = (): Record<string, string> => (auth.token ? { authorization: "Bearer " + auth.token } : {});

// A GET, or with a body a POST of it as JSON.
export async function api<T>(path: string, body?: unknown): Promise<T> {
  const headers = bearer();
  const opts: RequestInit = body === undefined ? { headers } : { method: "POST", headers: { ...headers, "content-type": "application/json" }, body: JSON.stringify(body) };
  const r = await fetch(path, opts);
  if (r.status === 401) { auth.refused(); throw new Error("401"); }
  if (!r.ok) {
    // The API answers {"error": "..."}; say that, not the JSON around it.
    const text = await r.text();
    let msg = text;
    try { msg = (JSON.parse(text) as { error?: string }).error || text; } catch { /* not JSON */ }
    throw new ApiError(msg || `${r.status} ${r.statusText}`, r.status);
  }
  return r.json() as Promise<T>;
}

// A page of a list the master filters and pages itself, with how many match
// in `x-total-count`.
export async function apiPage<T>(path: string): Promise<{ total: number; rows: T[] }> {
  const r = await fetch(path, { headers: bearer() });
  if (r.status === 401) { auth.refused(); throw new Error("401"); }
  if (!r.ok) throw new Error((await r.text()) || String(r.status));
  return { total: Number(r.headers.get("x-total-count") || 0), rows: (await r.json()) as T[] };
}

// A DELETE. It says what the master objected to, but a refused token here is
// just another error: these come from a click on something already shown.
export async function apiDelete(path: string): Promise<void> {
  const r = await fetch(path, { method: "DELETE", headers: { authorization: "Bearer " + auth.token } });
  if (!r.ok) {
    let m = await r.text();
    try { m = (JSON.parse(m) as { error?: string }).error || m; } catch { /* not JSON */ }
    throw new Error(m || String(r.status));
  }
}

// A DELETE whose answer nobody waits on: tidying up after a build.
export function deleteQuietly(path: string): Promise<Response> {
  return fetch(path, { method: "DELETE", headers: { authorization: "Bearer " + auth.token } });
}
