# Norbelys documentation

Mintlify site. Content directory: `apps/docs`.

- `index.mdx` — hello world
- `openapi.json` — public API contract (`crates/server/openapi.json`)
- `architecture.mdx` — package, feature, process and domain boundaries
- `development.mdx` — focused checks, isolated tests and explicit coverage
- `migrations.mdx` — external manual SQLx maintenance and runtime schema checks
- `observability.mdx` — optional OpenTelemetry and SigNoz configuration

```sh
bun run docs:dev
```
