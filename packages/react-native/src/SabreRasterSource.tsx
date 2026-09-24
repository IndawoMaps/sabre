import { type ReactNode, useEffect } from "react";
import { Layer, RasterSource, type RasterLayerSpecification } from "@maplibre/maplibre-react-native";

import { tileUrl, type Source } from "./api";
import { useEndpoint } from "./hooks";
import type { Style } from "./style";

export interface SabreRasterSourceProps {
  /** The MapLibre source id. The default layer is `${id}-layer`. */
  id: string;
  /** A file name in the file root, a `file://` URI inside it, or an `https://` URL. */
  source: Source;
  /** How to draw it. Changing it re-renders the visible tiles. */
  style?: Style;
  /** Tile size in pixels, 64 to 512. Default 256. */
  tileSize?: number;
  minzoom?: number;
  maxzoom?: number;
  attribution?: string;
  /** Paint for the default raster layer. */
  paint?: RasterLayerSpecification["paint"];
  /** Where the default layer goes in the stack. */
  beforeId?: string;
  afterId?: string;
  layerIndex?: number;
  /** Layers to draw from this source instead of the default raster layer. */
  children?: ReactNode;
  /** The server could not start. The source renders nothing until it can. */
  onError?: (error: Error) => void;
}

/**
 * A MapLibre raster source drawn by sabre, from a raster on the device (or
 * streamed over https). Starts sabre's in-app server on first use.
 *
 * MapLibre builds a raster source once and ignores later changes to its
 * tiles, so a new style -- or a server that came back on a new port --
 * remounts the source. That redraws every visible tile; the layer's ids
 * stay the same.
 */
export function SabreRasterSource({
  id, source, style, tileSize = 256, minzoom, maxzoom, attribution,
  paint, beforeId, afterId, layerIndex, children, onError,
}: SabreRasterSourceProps) {
  const { endpoint, error } = useEndpoint();
  useEffect(() => { if (error) onError?.(error); }, [error, onError]);
  if (!endpoint) return null;

  const url = tileUrl(endpoint, source, style, tileSize);
  return (
    <RasterSource key={url} id={id} tiles={[url]} tileSize={tileSize}
                  minzoom={minzoom} maxzoom={maxzoom} attribution={attribution}>
      {children ?? (
        <Layer type="raster" id={`${id}-layer`} paint={paint}
               beforeId={beforeId} afterId={afterId} layerIndex={layerIndex} />
      )}
    </RasterSource>
  );
}
