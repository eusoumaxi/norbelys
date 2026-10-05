# syntax=docker/dockerfile:1
# The per-host collector image, ghcr.io/<owner>/collector, built from builder.yaml with the
# OpenTelemetry Collector Builder and deployed by digest (COLLECTOR_DIGEST). It changes only when
# its inputs do, and the Images workflow builds it when they change or on a manual run:
#   docker build -f docker/collector/Dockerfile docker/collector
FROM golang:1.26-bookworm@sha256:a688600ca24f8a4d3ca77f95b0dd40704a9fc787c826660eb7ba0b641b8b175d AS build
WORKDIR /build
ENV GOTOOLCHAIN=local
RUN --mount=type=cache,target=/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build go install go.opentelemetry.io/collector/cmd/builder@v0.162.0
COPY builder.yaml .
RUN --mount=type=cache,target=/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 builder --config=builder.yaml

# A static binary: no libc, no shell, an unprivileged user.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab AS collector
COPY --from=build /build/_build/otelcol-norbelys /otelcol
USER nonroot
ENTRYPOINT ["/otelcol"]
CMD ["--config=/etc/otelcol/config.yaml"]
