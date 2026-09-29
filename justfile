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
