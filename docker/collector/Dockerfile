# syntax=docker/dockerfile:1
# The per-host collector image, ghcr.io/<owner>/collector, built from builder.yaml with the
# OpenTelemetry Collector Builder and deployed by digest (COLLECTOR_DIGEST). It changes only when
# its inputs do, and the Images workflow builds it when they change or on a manual run:
#   docker build -f docker/collector/Dockerfile docker/collector
FROM golang:1.25-bookworm@sha256:3b4a11519ad929d1e1d261a12cff056f0c85b735253d7d861346b9c6f8b36437 AS build
WORKDIR /build
RUN --mount=type=cache,target=/root/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build go install go.opentelemetry.io/collector/cmd/builder@v0.162.0
COPY builder.yaml .
RUN --mount=type=cache,target=/root/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 builder --config=builder.yaml

# A static binary: no libc, no shell, an unprivileged user.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab AS collector
COPY --from=build /build/_build/otelcol-norbelys /otelcol
USER nonroot
ENTRYPOINT ["/otelcol"]
CMD ["--config=/etc/otelcol/config.yaml"]
