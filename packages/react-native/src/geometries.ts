import { useSyncExternalStore } from "react";

import { getEndpoint } from "./server";

/** WGS84 clip geometry: WKT, or GeoJSON as text or as an object. */
export type Geometry = string | object;

/** Geometry by id: `{ id: geometry }`, a `Map`, or `[id, geometry]` pairs. */
export type GeometryEntries =
  | Record<string, Geometry>
  | Map<string | number, Geometry>
  | Iterable<[string | number, Geometry]>;

const revisions = new Map<string, number>();
const listeners = new Set<(provider: string, revision: number) => void>();

/**
 * Clip geometry, put in once and named by tiles and queries after that.
 *
 * A style's `mask` is WKT carried in the tile URL, on every tile MapLibre
 * asks for, and a farm of fields is more than a URL can hold. Put the
 * geometry here instead, and name it: `{ geometry_provider: "blocks",
 * geometry_id: ["19519", "19520"] }` -- the parameters a sabre server's
 * geometry providers take, so a style works against either.
 *
 * Geometry lives in the app's process for as long as it runs, across the
 * server restarting when the app comes back to the foreground.
 * `<SabreRasterSource>` redraws when geometry it names changes.
 */
export const geometries = {
  /** Add these, or overwrite them if the ids exist. Others are left alone. */
  set(provider: string, entries: GeometryEntries): Promise<number> {
    return change(provider, "POST", body(entries));
  },
  /** Make these the provider's whole contents, in one step. `{}` empties it. */
  replace(provider: string, entries: GeometryEntries): Promise<number> {
    return change(provider, "PUT", body(entries));
  },
  /** Remove these ids. Ids the provider does not have are ignored. */
  delete(provider: string, ids: Iterable<string | number>): Promise<number> {
    return change(provider, "DELETE", JSON.stringify([...ids].map(String)));
  },
  /** The provider's revision as of the last change that finished; 0 before any. */
  revision(provider: string): number {
    return revisions.get(provider) ?? 0;
  },
  /** Called with `(provider, revision)` after each change. Returns an unsubscribe function. */
  subscribe(listener: (provider: string, revision: number) => void): () => void {
    listeners.add(listener);
    return () => { listeners.delete(listener); };
  },
};

/** A provider's revision, as state: re-renders when its geometry changes. */
export function useGeometryRevision(provider: string | undefined): number {
  return useSyncExternalStore(
    (onChange) => geometries.subscribe((p) => { if (p === provider) onChange(); }),
    () => (provider ? geometries.revision(provider) : 0),
  );
}

function body(entries: GeometryEntries): string {
  const pairs = entries instanceof Map || Symbol.iterator in Object(entries)
    ? [...(entries as Iterable<[string | number, Geometry]>)]
    : Object.entries(entries);
  return JSON.stringify(Object.fromEntries(pairs.map(([id, g]) => [String(id), g])));
}

async function change(provider: string, method: string, payload: string): Promise<number> {
  const ep = await getEndpoint();
  const res = await fetch(`${ep.baseUrl}/geometries/${encodeURIComponent(provider)}`, {
    method,
    headers: { "Content-Type": "application/json" },
    body: payload,
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`sabre geometries: ${text}`);
  const { revision } = JSON.parse(text) as { revision: number };
  if (revision !== (revisions.get(provider) ?? 0)) {
    revisions.set(provider, revision);
    listeners.forEach((l) => l(provider, revision));
  }
  return revision;
}
