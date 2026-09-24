import { AppState } from "react-native";

import { native } from "./native";

/** Where the in-app tile server is listening. */
export interface Endpoint {
  port: number;
  /** Every route sits under it; without it the server answers 404. */
  token: string;
  /** `http://127.0.0.1:{port}/{token}`, the prefix of every route. */
  baseUrl: string;
  /** The directory `file://` sources resolve inside. */
  fileRoot: string;
}

export interface Options {
  /**
   * The directory local rasters are read from, and the only one. Default:
   * the app's files directory on Android (`Paths.document` in expo-file-system).
   */
  fileRoot?: string;
  /** Bytes of remote (`https://`) source data kept in memory. Default 64 MB. */
  cacheBytes?: number;
}

let options: Required<Pick<Options, "cacheBytes">> & Options = { cacheBytes: 64 * 1024 * 1024 };
let current: Endpoint | undefined;
let starting: Promise<Endpoint> | undefined;
let watching = false;
const listeners = new Set<() => void>();

/** Change where rasters come from, or the cache size. Only before the server starts. */
export function configure(next: Options): void {
  if (current || starting) {
    throw new Error("@sabremaps/react-native: configure() must be called before the server starts");
  }
  options = { ...options, ...next };
}

/**
 * The server's endpoint, starting it if it is not running. Every caller
 * shares one server.
 */
export function getEndpoint(): Promise<Endpoint> {
  if (current) return Promise.resolve(current);
  return (starting ??= start().finally(() => { starting = undefined; }));
}

async function start(): Promise<Endpoint> {
  const ep = await native().start(options.fileRoot ?? null, options.cacheBytes);
  set(ep);
  watchAppState();
  return ep;
}

function set(ep: Endpoint) {
  if (current?.baseUrl === ep.baseUrl) return;
  current = ep;
  listeners.forEach((l) => l());
}

// Back in the foreground, ask again. The native side returns the running
// server if it is still listening, or starts a new one -- on iOS the system
// reclaims a suspended app's sockets -- and a new port reaches every
// subscriber, which rebuilds its tile URLs.
function watchAppState() {
  if (watching) return;
  watching = true;
  AppState.addEventListener("change", (state) => {
    if (state === "active" && current) {
      start().catch(() => { /* the next getEndpoint() reports it */ });
    }
  });
}

/** The endpoint if the server has started, without starting it. */
export function currentEndpoint(): Endpoint | undefined {
  return current;
}

/** Called whenever the endpoint changes. Returns an unsubscribe function. */
export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** GeoTIFF file names directly inside the file root. */
export function listRasters(): string[] {
  return native().listRasters(current?.fileRoot ?? options.fileRoot ?? null);
}
