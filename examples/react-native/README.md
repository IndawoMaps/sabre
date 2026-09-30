# @sabremaps/react-native example

An Expo app showing GeoTIFFs from the device on a MapLibre map, fully offline
(the basemap is a plain background), with buttons to switch raster, mode,
colormap and stretch. It uses [`@sabremaps/react-native`](../../packages/react-native)
from the workspace: `App.tsx` is the whole integration.

## Setup: iOS

Xcode, with an iOS simulator runtime. CocoaPods comes from the repo's
`mise.toml`, with a prebuilt Ruby, since macOS's own is too old for it:

```sh
mise install
```

## Run on iOS

```sh
pnpm install
just rn-example-ios                  # SabreFFI.xcframework + package JS, then expo run:ios
just rn-push-ios data/ca.cog.tiff    # copy a raster into the booted simulator's app, then reload
```

`just rn-ios` rebuilds only the package's iOS library (after a change in `crates/`).

## Setup: Android

Toolchains come from the repo's `mise.toml` (JDK 17, Android cmdline-tools,
cargo-ndk). The rest of the SDK goes into mise's `ANDROID_HOME`:

```sh
mise install
yes | sdkmanager --licenses
sdkmanager "platform-tools" "platforms;android-35" "build-tools;35.0.0" \
  "ndk;27.1.12297006" "emulator" "system-images;android-35;google_apis;arm64-v8a"
avdmanager create avd -n sabre -k "system-images;android-35;google_apis;arm64-v8a"
```

If sdkmanager keeps failing with "Connection reset" on the large packages, the
archives can be fetched with a resumable `curl -C -` from the URLs in
`https://dl.google.com/android/repository/repository2-3.xml` and unpacked
into `$ANDROID_HOME` by hand.

## Run on Android

```sh
emulator -avd sabre &
pnpm install
just rn-example-android                 # native libraries + package JS, then expo run:android
just rn-push-android data/ca.cog.tiff   # copy a raster into the app, then reload
```

`just rn-android` rebuilds only the package (after a change in `crates/`), and
`pnpm --filter @sabremaps/react-native build` only its TypeScript.
