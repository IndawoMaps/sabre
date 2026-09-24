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

# ── Mobile (examples/react-native) ────────────────────────────────────────────
# Toolchains come from mise.toml: java, android-sdk, cargo-ndk. The NDK itself
# is installed with sdkmanager -- see examples/react-native/README.md.

mobile_module := "examples/react-native/modules/sabre"

# Build sabre-mobile for Android into the Expo module's jniLibs.
mobile-android profile="release":
    RUSTFLAGS="--remap-path-prefix={{justfile_directory()}}=sabre --remap-path-prefix=$HOME=~" \
        cargo ndk -t arm64-v8a -t x86_64 -o {{mobile_module}}/android/src/main/jniLibs \
        build -p sabre-mobile {{ if profile == "release" { "--release" } else { "" } }}
    @ls -la {{mobile_module}}/android/src/main/jniLibs/*/libsabre_mobile.so

# Build sabre-mobile for iOS (device + Apple silicon simulator) as SabreFFI.xcframework. Needs Xcode.
mobile-ios:
    rustup target add aarch64-apple-ios aarch64-apple-ios-sim
    cargo build -p sabre-mobile --release --target aarch64-apple-ios
    cargo build -p sabre-mobile --release --target aarch64-apple-ios-sim
    rm -rf {{mobile_module}}/ios/SabreFFI.xcframework
    xcodebuild -create-xcframework \
        -library target/aarch64-apple-ios/release/libsabre_mobile.a -headers crates/mobile/include \
        -library target/aarch64-apple-ios-sim/release/libsabre_mobile.a -headers crates/mobile/include \
        -output {{mobile_module}}/ios/SabreFFI.xcframework

# Copy a raster into the Android app's files directory (debug builds only).
mobile-push-android file:
    adb push {{file}} /data/local/tmp/
    adb shell run-as com.sabremaps.example cp /data/local/tmp/$(basename {{file}}) files/
    adb shell rm /data/local/tmp/$(basename {{file}})

# Build the Rust library, then build, install and launch the Android example.
mobile-run-android: mobile-android
    cd examples/react-native && pnpm exec expo run:android
