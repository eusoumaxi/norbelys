# syntax=docker/dockerfile:1
# The dashboard for self-hosting: the static build behind Caddy, which also forwards /api/*
# to the API (Caddyfile). Cloudflare serves the same build through worker/index.ts instead.
# Build from the repository root.
#
#   docker build -f apps/app/Dockerfile -t norbelys-app .
#   docker run --rm -p 5173:80 -e API_ORIGIN=http://host.docker.internal:3001 norbelys-app
#
# Base images are pinned by tag and index digest; bump both together.

FROM oven/bun:1.4.2-slim@sha256:cb3bbbb08e13a4a2ff400f24c7a2a1d5efa83f6ef8544d52d95a519631e2fc61 AS build
WORKDIR /src
# Every workspace manifest, so the lockfile verifies; only the app's dependencies install.
COPY package.json bun.lock ./
COPY patches patches
COPY apps/app/package.json apps/app/
COPY apps/web/package.json apps/web/
COPY apps/cli/package.json apps/cli/
COPY apps/docs/package.json apps/docs/
COPY sdks/typescript/package.json sdks/typescript/
RUN --mount=type=cache,id=bun-install,target=/root/.bun/install/cache bun install --frozen-lockfile --filter @norbelys/app
COPY sdks/typescript/src sdks/typescript/src
COPY brand brand
COPY apps/app apps/app
COPY crates/server/openapi.json crates/server/openapi.json
RUN bun run --cwd apps/app build

FROM caddy:2.11.4-alpine@sha256:6aeddd44c3078b0f9a35206472a11420648a79c184603ef95957d0a20044cb2b AS app
COPY apps/app/Caddyfile /etc/caddy/Caddyfile
COPY --from=build /src/apps/app/dist/client /srv
ENV API_ORIGIN=http://api:8080
EXPOSE 80
