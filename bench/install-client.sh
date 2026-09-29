#!/usr/bin/env bash
#
# Put bench-client on this machine.
#
#   bench/install-client.sh                                    # pull the published image
#   BENCH_CLIENT_IMAGE=ghcr.io/indawomaps/sabre-bench-client:sha-1a2b3c4 \
#       bench/install-client.sh
#   bench/install-client.sh --build                            # compile from source instead
#
# Pulling beats building here. Compiling 91 crates on a fresh 4-vCPU droplet is
# several minutes of a box that lives for an hour, and it means the generator
# is built by whatever toolchain that box happened to have rather than the one
# CI used.
#
# The binary is extracted from the image rather than run inside it: the suite
# drives it over ssh as a plain command with a plan on stdin, and a container
# in between would only add a layer to get that wrong in.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$REPO/target/release/bench-client"
IMAGE="${BENCH_CLIENT_IMAGE:-ghcr.io/indawomaps/sabre-bench-client:main}"
MODE=pull

[ "${1:-}" = "--build" ] && MODE=build

command -v docker >/dev/null || {
    echo "bench/install-client.sh: docker is required (to pull the image, or to build without a local toolchain)" >&2
    exit 1
}

if [ "$MODE" = build ]; then
    # In the same rust image CI uses, so no toolchain is needed here either.
    # Named volume for the registry: without it every build re-downloads every
    # crate, which on a box created minutes ago is most of the build.
    docker run --rm \
        -v "$REPO":/src \
        -v sabre-cargo-registry:/usr/local/cargo/registry \
        -w /src \
        "${RUST_IMAGE:-rust:1-slim}" \
        cargo build --release -p sabre-bench-client
else
    echo "pulling $IMAGE"
    docker pull -q "$IMAGE"

    # `docker create` makes a container without starting it, purely so its
    # filesystem can be read. ENTRYPOINT is never invoked.
    mkdir -p "$(dirname "$DEST")"
    cid="$(docker create "$IMAGE")"
    trap 'docker rm -f "$cid" >/dev/null 2>&1 || true' EXIT
    docker cp "$cid:/usr/local/bin/bench-client" "$DEST"
    chmod +x "$DEST"
fi

echo "installed $DEST"
"$DEST" --help | head -3
