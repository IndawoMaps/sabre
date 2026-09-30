set shell := ["bash", "-cu"]

default:
    @just --list

# File server only — serves the rasters in data/ at localhost:8080
serve:
    python3 -m http.server 8080 --directory data

# Native tile server only — tile API at localhost:8787, file:// sources from data/
native:
    cargo run --release -p sabre-server -- --file-root data

# Build the bare wasm into examples/wasm/pkg, for the pages there.
browser:
    cargo build -p sabre-browser --target wasm32-unknown-unknown --release
    cargo install -q wasm-bindgen-cli 2>/dev/null || true
    wasm-bindgen --target web --out-dir examples/wasm/pkg --no-typescript \
        target/wasm32-unknown-unknown/release/sabre_browser.wasm
    @printf 'wasm   %s bytes\n' "$(wc -c < examples/wasm/pkg/sabre_browser_bg.wasm | tr -d ' ')"
    @command -v brotli >/dev/null && printf 'brotli %s bytes\n' \
        "$(brotli -q 11 -c examples/wasm/pkg/sabre_browser_bg.wasm | wc -c | tr -d ' ')" || true

# Build the wasm into the @sabremaps/browser npm package, then link the workspace.
npm:
    # Rust embeds source paths in panic messages. Rewrite them, so what ships
    # names neither this machine's home directory nor where the crates live.
    RUSTFLAGS="--remap-path-prefix={{justfile_directory()}}=sabre --remap-path-prefix=$HOME=~" \
        cargo build -p sabre-browser --target wasm32-unknown-unknown --release
    wasm-bindgen --target web --out-dir packages/browser/wasm --no-typescript \
        target/wasm32-unknown-unknown/release/sabre_browser.wasm
    pnpm install

# The OpenLayers example, rendering in the browser through @sabremaps/ol.
example: npm
    cd examples/openlayers && pnpm dev --open /sabre.html

# Run the browser tests in a real browser (needs chromedriver on PATH).
browser-test:
    CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
        cargo test -p sabre-browser --target wasm32-unknown-unknown

# Benchmark sabre against OpenLayers' GeoTIFF source in Chrome. See bench/browser/README.md.
bench-browser *args: npm
    node bench/browser/run.ts {{args}}

# ── React Native (packages/react-native, examples/react-native) ─────────────
# Toolchains come from mise.toml: java, android-sdk, cargo-ndk. The NDK itself
# is installed with sdkmanager -- see examples/react-native/README.md.

rn_package := "packages/react-native"

# Build sabre-mobile for every Android ABI into @sabremaps/react-native, then its JS.
rn-android profile="release":
    rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
    # Rust embeds source paths in panic messages; keep this machine's out of what ships.
    RUSTFLAGS="--remap-path-prefix={{justfile_directory()}}=sabre --remap-path-prefix=$HOME=~" \
        cargo ndk -t arm64-v8a -t armeabi-v7a -t x86 -t x86_64 \
        -o {{rn_package}}/android/src/main/jniLibs \
        build -p sabre-mobile {{ if profile == "release" { "--release" } else { "" } }}
    # cargo-ndk copies every cdylib it built, dependencies' too (proj4rs has
    # one). Only sabre's is loaded; the rest would be dead weight in every app.
    find {{rn_package}}/android/src/main/jniLibs -name '*.so' ! -name libsabre_mobile.so -delete
    @ls -la {{rn_package}}/android/src/main/jniLibs/*/
    pnpm --filter @sabremaps/react-native build

# Build sabre-mobile for iOS (device + Apple silicon simulator) as SabreFFI.xcframework, then the package JS. Needs Xcode.
rn-ios:
    rustup target add aarch64-apple-ios aarch64-apple-ios-sim
    # Only the staticlib: built alongside the rlib, its dependencies stay LLVM
    # bitcode (from LTO), which Xcode's older LLVM cannot link. Alone, rustc
    # runs LTO itself and writes machine code. The paths are remapped as in
    # rn-android, and CFLAGS does the same for ring's C in the debug info.
    for target in aarch64-apple-ios aarch64-apple-ios-sim; do \
        RUSTFLAGS="--remap-path-prefix={{justfile_directory()}}=sabre --remap-path-prefix=$HOME=~" \
        CFLAGS="-ffile-prefix-map={{justfile_directory()}}=sabre -ffile-prefix-map=$HOME=~" \
            cargo rustc -p sabre-mobile --release --target $target --crate-type staticlib; \
    done
    rm -rf {{rn_package}}/ios/SabreFFI.xcframework
    xcodebuild -create-xcframework \
        -library target/aarch64-apple-ios/release/libsabre_mobile.a -headers crates/mobile/include \
        -library target/aarch64-apple-ios-sim/release/libsabre_mobile.a -headers crates/mobile/include \
        -output {{rn_package}}/ios/SabreFFI.xcframework
    @du -sh {{rn_package}}/ios/SabreFFI.xcframework/*/
    pnpm --filter @sabremaps/react-native build

# Copy a raster into the Android example's files directory (debug builds only).
rn-push-android file:
    adb push {{file}} /data/local/tmp/
    adb shell run-as com.sabremaps.example cp /data/local/tmp/$(basename {{file}}) files/
    adb shell rm /data/local/tmp/$(basename {{file}})

# Build the package, then build, install and launch the Android example.
rn-example-android: rn-android
    cd examples/react-native && pnpm exec expo run:android

# Copy a raster into the iOS example's Documents directory, on the booted simulator.
rn-push-ios file:
    cp {{file}} "$(xcrun simctl get_app_container booted com.sabremaps.example data)/Documents/"

# Build the package, then build, install and launch the iOS example on a simulator.
rn-example-ios: rn-ios
    cd examples/react-native && pnpm exec expo run:ios
