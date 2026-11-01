// The browser's storage, where reading or writing may throw: a private
// window or a locked-down browser refuses rather than stores, and the console
// then works the same, only forgetting.
//
// What is kept, and where:
//   sessionStorage  sandstorm-token                the API token, this tab only
//                   sandstorm-build                a build sandbox to come back to
//   localStorage    sandstorm-console-range        the time range, in seconds
//                   sandstorm-console-theme        "light" or "dark", if picked
//                   sandstorm-console-nodes-view   "history" for memory over time
//                   sandstorm-console-page-<name>  rows a page, per table
//                   sandstorm-console-pool         the overview's host group

function store(get: () => Storage) {
  return {
    get(key: string): string | null {
      try { return get().getItem(key); } catch { return null; }
    },
    set(key: string, value: string): void {
      try { get().setItem(key, value); } catch { /* not kept */ }
    },
    remove(key: string): void {
      try { get().removeItem(key); } catch { /* not kept */ }
    },
  };
}

export const local = store(() => localStorage);
export const session = store(() => sessionStorage);

// Twenty a page by default, for every table: about a screenful.
export const PAGE_SIZES = [20, 50, 100];

// Shared by every table. The size a page holds is remembered per table, in
// this browser only. Each table may offer its own sizes; the default is one
// of them.
export function pageSize(name: string, dflt: number, sizes: number[] = PAGE_SIZES): number {
  const v = Number(local.get("sandstorm-console-page-" + name));
  return sizes.includes(v) ? v : dflt;
}
export function keepPageSize(name: string, v: number): void {
  local.set("sandstorm-console-page-" + name, String(v));
}

// The time range the charts cover, which also decides whether times on them
// carry seconds. Kept here, apart from the rest of the state, because the
// formatting helpers read it too.
export const view = { range: Number(local.get("sandstorm-console-range")) || 900 };
