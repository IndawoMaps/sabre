# sabre in React Native

Offline GeoTIFFs on a MapLibre map in an Expo app, with styles you can change at
runtime. **Spike:** Android is built and run from this repo; iOS is written but
has not been built yet (it needs Xcode).

## How it works

MapLibre Native reads raster tiles only from URL templates, so the app runs
sabre's tile server itself, on the phone's loopback interface:

```
App.tsx ──Sabre.start()──▶ modules/sabre (Kotlin / Swift)
                              │  JNI / C ABI
                              ▼
                   crates/mobile ── sabre-server router ── sabre-core
                   127.0.0.1:{random port}/{token}/tiles/{z}/{x}/{y}?url=file://dem.tif&colormap=…
                              ▲
        MapLibre Native ──────┘  (RasterSource tiles=[that template])
```

- `file://` sources resolve inside the app's files directory (Android) or
  Documents (iOS), and nowhere else.
- Every route sits under a random per-launch token, because any app on an
  Android device can connect to a loopback port.
- Tiles are sent `Cache-Control: no-store`, so MapLibre's disk cache does
  not keep a copy of every tile in every style.
- **Restyling = a new tile URL.** MapLibre builds a raster source once and ignores
  later changes to `tiles`, so the example keys the `RasterSource` (and its `id`)
  by URL. A new style remounts the source and every visible tile re-renders.
- `modules/sabre/app.plugin.js` allows cleartext HTTP to `127.0.0.1` only
  (Android network security config) and sets `NSAllowsLocalNetworking` (iOS).

## Setup

Toolchains come from the repo's `mise.toml` (JDK 17, Android cmdline-tools,
cargo-ndk). The rest of the SDK goes into mise's `ANDROID_HOME`:

```sh
mise install
yes | sdkmanager --licenses
sdkmanager "platform-tools" "platforms;android-35" "build-tools;35.0.0" \
  "ndk;27.1.12297006" "emulator" "system-images;android-35;google_apis;arm64-v8a"
rustup target add aarch64-linux-android x86_64-linux-android
avdmanager create avd -n sabre -k "system-images;android-35;google_apis;arm64-v8a"
```

## Run (Android)

```sh
emulator -avd sabre &
pnpm install
just mobile-run-android                    # builds libsabre_mobile.so, then expo run:android
just mobile-push-android data/ca.cog.tiff  # then reload the app
```

## iOS (not yet built)

```sh
just mobile-ios                            # SabreFFI.xcframework, needs Xcode
cd examples/react-native && pnpm exec expo run:ios
```

Rasters go in the app's Documents directory (for the simulator,
`xcrun simctl get_app_container booted com.sabremaps.example data`).
