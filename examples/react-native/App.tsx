import { useEffect, useMemo, useState } from "react";
import { Pressable, ScrollView, StyleSheet, Text, View } from "react-native";
import { StatusBar } from "expo-status-bar";
import { Camera, Layer, Map, RasterSource, type StyleSpecification } from "@maplibre/maplibre-react-native";

import * as Sabre from "./modules/sabre";

// Nothing but a background: the point is that the map works with no network.
const OFFLINE_STYLE: StyleSpecification = {
  version: 8,
  sources: {},
  layers: [{ id: "background", type: "background", paint: { "background-color": "#1b1d22" } }],
};

const COLORMAPS = ["viridis", "turbo", "spectral", "rdylbu", "greys", "hot"];
const MODES = ["colormap", "hillshade", "classified"] as const;
type Mode = (typeof MODES)[number];

export default function App() {
  const [endpoint, setEndpoint] = useState<Sabre.Endpoint>();
  const [error, setError] = useState<string>();
  const [files, setFiles] = useState<string[]>([]);
  const [file, setFile] = useState<string>();
  const [info, setInfo] = useState<Sabre.RasterInfo>();

  const [mode, setMode] = useState<Mode>("colormap");
  const [colormap, setColormap] = useState("viridis");
  // How much of the data range the colours span, from the bottom: a quick
  // way to see a restyle change something other than the palette.
  const [stretch, setStretch] = useState(1);

  useEffect(() => {
    Sabre.start()
      .then((ep) => {
        setEndpoint(ep);
        const found = Sabre.listRasters(ep.fileRoot);
        setFiles(found);
        setFile(found[0]);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    if (!endpoint || !file) return;
    setInfo(undefined);
    Sabre.info(endpoint, file).then(setInfo).catch((e) => setError(String(e)));
  }, [endpoint, file]);

  const style = useMemo<Sabre.Style | undefined>(() => {
    if (!info) return undefined;
    const lo = info.stats_min ?? 0;
    const hi = lo + ((info.stats_max ?? 255) - lo) * stretch;
    const nodata = info.nodata ?? undefined;
    switch (mode) {
      case "colormap":
        return { mode, colormap, min: lo, max: hi, nodata };
      case "hillshade":
        return { mode, hillshade_colormap: colormap, min: lo, max: hi, nodata };
      case "classified": {
        const step = (hi - lo) / 4;
        return { mode, nodata, stops: [
          [lo, "#2c7bb6"], [lo + step, "#abd9e9"], [lo + 2 * step, "#ffffbf"],
          [lo + 3 * step, "#fdae61"], [hi, "#d7191c"],
        ] };
      }
    }
  }, [info, mode, colormap, stretch]);

  const tiles = endpoint && file && style ? Sabre.tileUrl(endpoint, file, style) : undefined;

  if (error) {
    return <Centered><Text style={styles.error}>{error}</Text></Centered>;
  }
  if (!endpoint) {
    return <Centered><Text style={styles.text}>Starting sabre…</Text></Centered>;
  }
  if (files.length === 0) {
    return (
      <Centered>
        <Text style={styles.text}>No .tif files in {endpoint.fileRoot}</Text>
        <Text style={styles.hint}>Push one with `just mobile-push-android data/ca.cog.tiff`</Text>
      </Centered>
    );
  }

  return (
    <View style={styles.root}>
      <StatusBar style="light" />
      {info && (
        // Keyed by file so the initial camera applies to each raster.
        <Map key={file} style={styles.map} mapStyle={OFFLINE_STYLE}>
          <Camera initialViewState={{ bounds: [info.extent.west, info.extent.south, info.extent.east, info.extent.north] }} />
          {tiles && (
            // MapLibre builds a raster source once and ignores later changes
            // to `tiles`, so a new style is a new source: key and id by URL.
            <RasterSource key={tiles} id={`sabre-${hash(tiles)}`} tiles={[tiles]} tileSize={256}>
              <Layer type="raster" id={`sabre-layer-${hash(tiles)}`} paint={{ "raster-fade-duration": 0 }} />
            </RasterSource>
          )}
        </Map>
      )}
      <View style={styles.panel}>
        <Chips options={files} value={file} onChange={setFile} />
        <Chips options={[...MODES]} value={mode} onChange={(m) => setMode(m as Mode)} />
        {mode !== "classified" && <Chips options={COLORMAPS} value={colormap} onChange={setColormap} />}
        <Chips options={["25%", "50%", "100%"]} value={`${stretch * 100}%`}
               onChange={(s) => setStretch(parseInt(s, 10) / 100)} />
        {info && (
          <Text style={styles.hint}>
            {info.width}×{info.height} {info.dtype} · {info.stats_min?.toFixed(1)}–{info.stats_max?.toFixed(1)} · port {endpoint.port}
          </Text>
        )}
      </View>
    </View>
  );
}

function Chips({ options, value, onChange }: { options: string[]; value?: string; onChange: (v: string) => void }) {
  return (
    <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerStyle={styles.chips}>
      {options.map((o) => (
        <Pressable key={o} onPress={() => onChange(o)} style={[styles.chip, o === value && styles.chipOn]}>
          <Text style={[styles.chipText, o === value && styles.chipTextOn]}>{o}</Text>
        </Pressable>
      ))}
    </ScrollView>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return <View style={[styles.root, styles.centered]}>{children}</View>;
}

/** Short, stable id for a URL: source and layer ids must be unique per style. */
function hash(s: string): string {
  let h = 5381;
  for (let i = 0; i < s.length; i++) h = ((h * 33) ^ s.charCodeAt(i)) >>> 0;
  return h.toString(36);
}

const styles = StyleSheet.create({
  root: { flex: 1, backgroundColor: "#1b1d22" },
  centered: { alignItems: "center", justifyContent: "center", padding: 24, gap: 8 },
  map: { flex: 1 },
  panel: { paddingTop: 8, paddingBottom: 32, gap: 8, backgroundColor: "#121317" },
  chips: { gap: 6, paddingHorizontal: 12 },
  chip: { paddingHorizontal: 12, paddingVertical: 6, borderRadius: 14, backgroundColor: "#262930" },
  chipOn: { backgroundColor: "#e8eaed" },
  chipText: { color: "#c7cad1", fontSize: 13 },
  chipTextOn: { color: "#121317" },
  text: { color: "#e8eaed", fontSize: 15, textAlign: "center" },
  hint: { color: "#8a8f98", fontSize: 12, paddingHorizontal: 12 },
  error: { color: "#ff8a80", fontSize: 14 },
});
