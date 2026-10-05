# Norbelys

Norbelys connects mailboxes and relays, sends paced campaigns, and processes replies
and delivery evidence. The monorepo contains a Rust backend and CLI, a TypeScript
dashboard, and TypeScript and Python SDKs.

- [Architecture](apps/docs/architecture.mdx): packages, feature ownership and process roles.
- [Development](apps/docs/development.mdx): prerequisites, focused tests and explicit coverage.
- [Database maintenance](apps/docs/migrations.mdx): external manual SQLx migrations and release checks.
- [Observability](apps/docs/observability.mdx): optional OpenTelemetry and SigNoz.
- [Contributing](CONTRIBUTING.md): source, contracts and review expectations.

Use the Rust toolchain in `rust-toolchain.toml`, Bun from `package.json`, and uv with
Python 3.11 or newer. Install locked dependencies, then run the static checks:

```sh
bun install --frozen-lockfile
uv sync --frozen --package norbelys
bun run check
```

This check needs neither PostgreSQL nor Docker. Database tests require an explicitly
prepared disposable local cluster; see the development guide before running them.

For a local container installation, `bun run selfhost:setup` prepares private
settings and pinned images. It does not run migrations or start the application.
Follow the database maintenance guide for explicit preparation and startup. Images
must be available to your registry account or published for anonymous access.

Production provisioning and rollout belong to the installation's Terraform
repository. This repository contains portable product source and examples, not
private infrastructure or credentials.

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for attribution and the
separate treatment of names and trademarks.
