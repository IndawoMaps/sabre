# Two images from one compile.
#
#   docker build -t sabre .                          # the server (default target)
#   docker build --target bench-client -t bc .       # the load generator
#
# Both binaries come out of the same `cargo build`, so CI compiles the
# workspace once and publishes two runtime images from it. The `server` stage
# is last on purpose: it is what a bare `docker build .` should produce.

FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# All of crates/, sabre-browser included: it is not built here, but Cargo
# refuses to load a workspace with a member missing.
COPY crates crates
COPY bench/client bench/client
COPY LICENSE.md ./
RUN cargo build --release -p sabre-server -p sabre-bench-client

# The benchmark load generator. Published so a benchmark box can pull it
# rather than spend six minutes compiling 91 crates on a droplet that will be
# destroyed within the hour.
FROM debian:bookworm-slim AS bench-client
COPY --from=build /src/target/release/bench-client /usr/local/bin/bench-client
ENTRYPOINT ["bench-client"]

FROM debian:bookworm-slim AS server
COPY --from=build /src/target/release/sabre-server /usr/local/bin/sabre-server
ENV SABRE_BIND=0.0.0.0:8787
EXPOSE 8787
ENTRYPOINT ["sabre-server"]
