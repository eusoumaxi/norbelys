import { expect, test } from "bun:test";

import { formatSelection, selectChecks } from "./check-scope.mjs";

test("a deleted or added SDK operation checks its language and consumers", () => {
  expect(
    selectChecks(["sdks/typescript/src/generated/resources.ts"])
  ).toMatchObject({
    backend: false,
    typescript: true,
    python: false,
    frontends: ["app"],
    backend_mode: "workspace",
  });
  expect(selectChecks(["sdks/python/src/norbelys/errors.py"]).python).toBe(
    true
  );
});
test("CLI changes run native checks without starting unrelated database suites", () => {
  expect(selectChecks(["crates/cli/src/main.rs"]).backend_mode).toBe("cli");
  expect(
    selectChecks(["crates/cli/src/main.rs", "Cargo.lock"]).backend_mode
  ).toBe("workspace");
});
test("native job modes reach Actions without JSON quotes", () => {
  const output = formatSelection(selectChecks(["crates/cli/src/main.rs"]));
  expect(output).toContain("backend_mode=cli\n");
  expect(output).toContain("backend=true\n");
  expect(output).toContain("frontends=[]\n");
});
test("a cross-component rename includes both paths", () => {
  const result = selectChecks(["apps/app/old.ts", "apps/web/new.ts"]);
  expect(result.frontends).toEqual(["app", "web"]);
  expect(result.backend).toBe(false);
});
test("shared contracts check SDKs and the backend, but a CLI worker change stays independent", () => {
  const contract = selectChecks(["crates/server/openapi.json"]);
  expect(contract.backend && contract.typescript && contract.python).toBe(true);
  expect(selectChecks(["apps/cli/src/index.ts"]).frontends).toEqual([
    "cli-downloads",
  ]);
});
test("an unavailable baseline fails open to every check", () => {
  expect(selectChecks(null)).toEqual(
    selectChecks([".github/workflows/ci.yml"])
  );
  expect(selectChecks(null).frontends).toEqual(["app", "cli-downloads", "web"]);
  expect(selectChecks(["LICENSE"]).frontends).toEqual([]);
});
test("collector and image publication changes do not compile unrelated products", () => {
  for (const path of [
    "docker/collector/Dockerfile",
    "docker/collector/builder.yaml",
    ".github/workflows/images.yml",
  ]) {
    expect(selectChecks([path])).toMatchObject({
      backend: false,
      typescript: false,
      python: false,
      frontends: [],
      audits: [],
    });
  }
});
test("language locks and workflows select their own consumers and audits", () => {
  expect(selectChecks(["uv.lock"])).toMatchObject({
    backend: false,
    typescript: false,
    python: true,
    frontends: [],
    audits: ["python"],
  });
  expect(selectChecks(["Cargo.lock"])).toMatchObject({
    backend: true,
    typescript: false,
    python: false,
    frontends: [],
    audits: ["rust"],
  });
  expect(selectChecks([".github/workflows/sdk.yml"])).toMatchObject({
    backend: false,
    typescript: true,
    python: false,
    frontends: [],
  });
  expect(selectChecks([".github/workflows/backend.yml"])).toMatchObject({
    backend: true,
    typescript: false,
    python: false,
    frontends: [],
  });
});
test("formatting and database fixture changes do not run each other's native jobs", () => {
  expect(selectChecks(["rustfmt.toml"])).toMatchObject({
    backend: true,
    backend_mode: "gate",
  });
  expect(selectChecks(["docker/services.yml"])).toMatchObject({
    backend: true,
    backend_mode: "database",
  });
  expect(selectChecks(["rustfmt.toml", "docker/services.yml"])).toMatchObject({
    backend: true,
    backend_mode: "workspace",
  });
  expect(selectChecks(["dist-workspace.toml"])).toMatchObject({
    backend: true,
    backend_mode: "cli",
  });
});
test("self-host tooling checks its regression fixtures without starting PostgreSQL", () => {
  expect(
    selectChecks(["scripts/selfhost.ts", "docker/compose.yml"])
  ).toMatchObject({
    tooling: true,
    javascript: true,
    backend: false,
    frontends: [],
    audits: [],
  });
});
test("dashboard relay changes retain security regression tests", () => {
  expect(selectChecks(["apps/app/worker/telemetry.ts"])).toMatchObject({
    tooling: true,
    javascript: true,
    backend: false,
    frontends: ["app"],
  });
});
test("ordinary web content does not validate SDKs or native code", () => {
  expect(selectChecks(["apps/web/src/pages/about.astro"])).toMatchObject({
    backend: false,
    typescript: false,
    python: false,
    frontends: ["web"],
    audits: [],
  });
});
