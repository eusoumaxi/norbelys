import { expect, test } from "bun:test";

import { selectChecks } from "./check-scope.mjs";

test("a deleted or added SDK operation checks its language and consumers", () => {
  expect(selectChecks(["sdks/typescript/src/generated/resources.ts"])).toEqual({
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
  expect(selectChecks(null)).toEqual(selectChecks(["turbo.json"]));
  expect(selectChecks(null).frontends).toEqual(["app", "cli-downloads", "web"]);
  expect(selectChecks(["LICENSE"]).frontends).toEqual([]);
});
test("native lint configuration and self-host inputs exercise backend checks", () => {
  for (const path of [
    "rustfmt.toml",
    "clippy.toml",
    "dist-workspace.toml",
    "docker/compose.yml",
    "docker/collector.yml",
    "scripts/selfhost.ts",
  ]) {
    expect(selectChecks([path]).backend_mode).toBe("workspace");
    expect(selectChecks([path]).backend).toBe(true);
  }
});
