import { useEffect, useLayoutEffect, useRef } from "react";
import { apiPage, ApiError, auth, authGeneration } from "../api";
import { st, useStore, type Tab } from "../state";

interface Page<T> { total: number; rows: T[] }
export function usePagedList<T>(
  tab: Tab,
  path: string,
  poll: boolean,
  onPage: (page: Page<T>, signal: AbortSignal, isCurrent: () => boolean) => void | Promise<void>,
  onError: (error: unknown) => void,
): () => void {
  useStore();
  const active = st.tab === tab && !!auth.token;
  const session = authGeneration;
  const refreshed = st.refreshed;
  const handlers = useRef({ onPage, onError });
  const load = useRef<(replace?: boolean) => void>(() => {});

  useLayoutEffect(() => { handlers.current = { onPage, onError }; });
  useLayoutEffect(() => {
    if (!active) return;
    let request: AbortController | null = null;
    load.current = (replace = false) => {
      if (st.tab !== tab || authGeneration !== session) return;
      if (request && !replace) return; // Coalesce polls for an unchanged query.
      request?.abort();
      const controller = new AbortController();
      request = controller;
      const { signal } = controller;
      const isCurrent = () => !signal.aborted && authGeneration === session && st.tab === tab;
      void (async () => {
        try {
          const page = await apiPage<T>(path, signal);
          if (isCurrent()) await handlers.current.onPage(page, signal, isCurrent);
        } catch (error) {
          if (!isCurrent()) return;
          if (error instanceof ApiError && error.status === 401) auth.refused();
          else handlers.current.onError(error);
        } finally {
          if (request === controller) request = null;
        }
      })();
    };
    load.current();
    return () => {
      request?.abort();
      load.current = () => {};
    };
  }, [active, path, session, tab]);

  useEffect(() => {
    if (poll && refreshed) load.current();
  }, [poll, refreshed]);

  // Reload after a mutation even if a read of that same query was pending.
  return () => load.current(true);
}
