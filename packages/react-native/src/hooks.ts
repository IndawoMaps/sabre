import { useEffect, useState, useSyncExternalStore } from "react";

import { info, type RasterInfo, type Source } from "./api";
import { currentEndpoint, type Endpoint, getEndpoint, subscribe } from "./server";

/**
 * The server's endpoint, starting it on first use. Re-renders when it
 * changes, which it does if the server had to restart.
 */
export function useEndpoint(): { endpoint?: Endpoint; error?: Error } {
  const endpoint = useSyncExternalStore(subscribe, currentEndpoint);
  const [error, setError] = useState<Error>();
  useEffect(() => {
    getEndpoint().catch((e) => setError(e instanceof Error ? e : new Error(String(e))));
  }, []);
  return { endpoint, error };
}

/** `info()` for `source`, as state. */
export function useRasterInfo(source: Source | undefined): { info?: RasterInfo; error?: Error; loading: boolean } {
  const [state, setState] = useState<{ info?: RasterInfo; error?: Error; loading: boolean }>({ loading: !!source });
  useEffect(() => {
    if (!source) {
      setState({ loading: false });
      return;
    }
    let live = true;
    setState({ loading: true });
    info(source)
      .then((i) => live && setState({ info: i, loading: false }))
      .catch((e) => live && setState({ error: e instanceof Error ? e : new Error(String(e)), loading: false }));
    return () => { live = false; };
  }, [source]);
  return state;
}
