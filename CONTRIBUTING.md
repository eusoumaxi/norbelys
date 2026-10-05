# Contributing

Read the [architecture map](apps/docs/architecture.mdx) and
[development guide](apps/docs/development.mdx) before changing a subsystem. Keep
code, errors, comments, tests and documentation in English. Follow the repository's
Rust, TypeScript and Python formatters and linters.

Keep feature operations with their queries and behavior tests. Put deterministic
policy in `crates/server/src/domain/`, and initialize dependencies and process
supervision in `roles/`. Reuse existing pagination, authorization, idempotency,
network policy and delivery contracts. Add an abstraction only when it owns a real
invariant or useful boundary.

Choose checks by changed behavior. Use `bun run check` for static feedback and
package/module tests for the affected behavior. A database change needs real
PostgreSQL verification; a formatting change does not. Batch related edits and
reuse passing results while their relevant inputs remain unchanged. Full tests and
coverage are explicit commands, not prerequisites for every small edit.

For a schema change, add a numbered SQLx migration and preserve already applied
files. Do not edit migration history or introduce automatic runtime migrations.
Refresh committed SQLx metadata on the isolated fixture when queries change. See
[manual maintenance](apps/docs/migrations.mdx) for the shared sequence and guards.

For a public API change, update the Rust HTTP definitions, regenerate the public
OpenAPI contract and affected SDK/docs outputs, and verify runtime authorization as
well as schema consistency. Keep generated files deterministic; do not patch a
generated copy independently.

Explain the user-visible problem and resulting behavior in a change description.
List focused verification and any limits. Preserve negative authorization,
concurrency, recovery and resource-bound guarantees when consolidating tests.
Do not include credentials, private installation addresses, Terraform state or
customer/provider payloads in examples or test logs. Report suspected security
issues privately through a verified repository-owner channel; do not put sensitive
details in a public issue or pull request.
