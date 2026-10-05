# syntax=docker/dockerfile:1
# Rust runtime images. CI publishes sha-<commit> tags with SBOM and provenance attestations;
# installations select immutable digests. To build an individual target locally:
#
#   docker build --target server .            ghcr.io/<owner>/server            (the default target)
#   docker build --target server-analytics .  ghcr.io/<owner>/server-analytics  (DuckDB bundled)
#   docker build --target smtp .              ghcr.io/<owner>/smtp              (the managed MTA's control plane)
#
# One binary, `norbelys-server`, holds every role; a container picks its role with its command
# (`CMD ["/app/norbelys-server", "api"]` by default) and probes its own health with
# `norbelys-server healthcheck <url>`, since the runtime image has no shell and no curl. The
# analytics role reads Parquet with DuckDB, which is large to compile and to ship, so only its
# own image is built with the `analytics` feature. The `norbelys` CLI is not in any image.
#
# Compiled dependencies survive between builds in BuildKit cache mounts (the registry, and one
# target directory per image, since their features differ), so a source change recompiles the
# workspace's crates and links; executables are copied out of the cache mount into the image.
# The sqlx macros check every query against the committed metadata in .sqlx/, never a database.
# Base images are pinned by tag and digest; bump both together, and the Rust tag with
# rust-toolchain.toml.

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS source
WORKDIR /src
ENV SQLX_OFFLINE=true \
    CARGO_TERM_COLOR=never
COPY . .
# Docker initializes named volumes from these directories with the unprivileged owner's
# permissions. Without them a fresh spool volume is root-owned and the role cannot start.
RUN install -d -m 0750 /state/ingress-spool /state/tracking-spool /state/objects

FROM source AS build-server
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=target-server,target=/src/target \
    cargo build --release --locked -p norbelys-server --bin norbelys-server \
    && install -D -m 0755 target/release/norbelys-server /out/norbelys-server

FROM source AS build-server-analytics
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=target-server-analytics,target=/src/target \
    cargo build --release --locked -p norbelys-server --bin norbelys-server --features analytics \
    && install -D -m 0755 target/release/norbelys-server /out/norbelys-server

FROM source AS build-smtp
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=target-smtp,target=/src/target \
    cargo build --release --locked -p norbelys-smtp --bin norbelys-smtp \
    && install -D -m 0755 target/release/norbelys-smtp /out/norbelys-smtp

# glibc, libgcc, libstdc++ (DuckDB) and CA certificates; no shell, no package manager; runs as
# the unprivileged `nonroot` user (uid 65532). The allocator keeps at most two arenas: glibc's
# default of eight per core multiplies the memory a small container holds.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
COPY --from=source --chown=65532:65532 /state/ /var/lib/norbelys/
ENV GLIBC_TUNABLES=glibc.malloc.arena_max=2
USER nonroot

FROM runtime AS smtp
COPY --from=build-smtp /out/norbelys-smtp /app/norbelys-smtp
CMD ["/app/norbelys-smtp", "serve"]

FROM runtime AS server-analytics
COPY --from=build-server-analytics /out/norbelys-server /app/norbelys-server
CMD ["/app/norbelys-server", "analytics"]

FROM runtime AS server
COPY --from=build-server /out/norbelys-server /app/norbelys-server
EXPOSE 8080
CMD ["/app/norbelys-server", "api"]
