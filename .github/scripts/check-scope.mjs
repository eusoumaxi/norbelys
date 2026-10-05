/** Select checks from both sides of a rename/deletion; an unknown baseline runs every suite. */
import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";

import { successfulBase } from "./successful-base.mjs";

const nativeChecks = (path, native) => {
  if (
    path.startsWith("crates/cli/") ||
    [
      "dist-workspace.toml",
      ".github/scripts/install-rustup.sh",
      ".github/workflows/norbelys-cli-release.yml",
    ].includes(path)
  ) {
    native.add("cli");
  } else if (
    ["crates/", "tools/xtask/", ".cargo/", ".sqlx/"].some((prefix) =>
      path.startsWith(prefix)
    ) ||
    [
      "Cargo.toml",
      "Cargo.lock",
      "rust-toolchain.toml",
      ".env.example",
      "scripts/rust-coverage.mjs",
      ".github/workflows/backend.yml",
    ].includes(path)
  ) {
    native.add("workspace");
  }
  if (["rustfmt.toml", "clippy.toml"].includes(path)) {
    native.add("gate");
  }
  if (
    [
      "docker/services.yml",
      "docker/coverage.yml",
      "scripts/test-database.mjs",
    ].includes(path)
  ) {
    native.add("database");
  }
};

const dependencyAudits = (path, audits) => {
  if (path === ".github/workflows/ci.yml") {
    for (const ecosystem of ["javascript", "python", "rust"]) {
      audits.add(ecosystem);
    }
  }
  if (
    path === "Cargo.lock" ||
    path.endsWith("Cargo.toml") ||
    path === "deny.toml"
  ) {
    audits.add("rust");
  }
  if (path === "uv.lock" || path.endsWith("pyproject.toml")) {
    audits.add("python");
  }
  if (
    path === "bun.lock" ||
    path.endsWith("package.json") ||
    path.startsWith(".github/scripts/audit-dependencies") ||
    path.startsWith("patches/")
  ) {
    audits.add("javascript");
  }
};

const sdkAndFrontendChecks = (path, result, apps) => {
  if (
    [
      "uv.lock",
      "pyproject.toml",
      ".python-version",
      ".github/workflows/python.yml",
    ].includes(path) ||
    path.startsWith("sdks/python/")
  ) {
    result.python = true;
  }
  if (path === ".github/workflows/frontend.yml") {
    for (const app of ["app", "web", "cli-downloads"]) {
      apps.add(app);
    }
  }
  if (
    path.startsWith("sdks/typescript/") ||
    [
      ".github/workflows/sdk.yml",
      ".github/scripts/check-coverage.mjs",
    ].includes(path)
  ) {
    result.typescript = true;
    if (path.startsWith("sdks/typescript/")) {
      apps.add("app");
    }
  }
  if (
    path.startsWith("tools/codegen/") ||
    path === "crates/server/openapi.json"
  ) {
    result.typescript = true;
    result.python = true;
    apps.add("app");
  }
  for (const app of ["app", "web", "cli"]) {
    if (path.startsWith(`apps/${app}/`)) {
      apps.add(app === "cli" ? "cli-downloads" : app);
    }
  }
  if (path.startsWith("brand/")) {
    result.tooling = true;
    apps.add("app");
    apps.add("web");
  }
};

export const selectChecks = (paths) => {
  const result = {
    backend: false,
    typescript: false,
    python: false,
    tooling: false,
    javascript: false,
    frontends: [],
  };
  const apps = new Set();
  const native = new Set();
  const audits = new Set();
  const javascript = () => {
    result.javascript = true;
    result.tooling = true;
    result.typescript = true;
    for (const app of ["app", "web", "cli-downloads"]) {
      apps.add(app);
    }
  };
  const all = () => {
    native.add("workspace");
    javascript();
    result.python = true;
  };
  if (paths === null) {
    all();
    for (const ecosystem of ["javascript", "python", "rust"]) {
      audits.add(ecosystem);
    }
  }
  for (const path of paths ?? []) {
    nativeChecks(path, native);
    dependencyAudits(path, audits);
    sdkAndFrontendChecks(path, result, apps);
    if (
      [
        "package.json",
        "bun.lock",
        "turbo.json",
        ".editorconfig",
        ".github/workflows/ci.yml",
        ".github/actions/setup/action.yml",
      ].includes(path)
    ) {
      all();
    }
    if (path.startsWith("patches/")) {
      javascript();
    }
    if (
      path.startsWith(".github/") ||
      path.startsWith("scripts/") ||
      path.startsWith("tools/codegen/") ||
      path.startsWith("apps/app/worker/") ||
      path === "apps/app/src/lib/telemetry-policy.ts" ||
      [
        "tsconfig.tools.json",
        "oxlint.config.ts",
        "oxfmt.config.ts",
        "docker/compose.yml",
      ].includes(path)
    ) {
      result.tooling = true;
      result.javascript = true;
    }
    if (
      /\.(?:[cm]?[jt]sx?|astro|json|ya?ml)$/u.test(path) &&
      !path.startsWith(".sqlx/") &&
      !path.startsWith("crates/")
    ) {
      result.javascript = true;
    }
  }
  result.frontends = [...apps].toSorted();
  result.backend = native.size > 0;
  result.backend_mode = native.size === 1 ? [...native][0] : "workspace";
  result.audits = [...audits].toSorted();
  return result;
};

/** Actions scalar inputs are literal strings; only matrices need JSON serialization. */
export const formatSelection = (selected) =>
  Object.entries(selected)
    .map(
      ([key, value]) =>
        `${key}=${typeof value === "string" ? value : JSON.stringify(value)}\n`
    )
    .join("");

export const changedPaths = (base, head) => {
  if (!base || /^0+$/u.test(base)) {
    return null;
  }
  try {
    return execFileSync(
      "git",
      ["diff", "--name-only", "--no-renames", "-z", base, head],
      { encoding: "utf-8" }
    )
      .split("\0")
      .filter(Boolean);
  } catch {
    return null;
  }
};
if (import.meta.main) {
  const event = JSON.parse(
    readFileSync(process.env.GITHUB_EVENT_PATH, "utf-8")
  );
  const head = event.pull_request?.head.sha ?? process.env.GITHUB_SHA;
  const base =
    event.pull_request?.base.sha ??
    (process.env.GITHUB_EVENT_NAME === "push"
      ? successfulBase("ci.yml", head)
      : null);
  const selected =
    process.env.GITHUB_EVENT_NAME === "schedule"
      ? { ...selectChecks([]), audits: ["javascript", "python", "rust"] }
      : selectChecks(changedPaths(base, head));
  console.log(
    `Comparison baseline: ${base ?? "none; run every applicable check"}`
  );
  const output = formatSelection(selected);
  process.stdout.write(output);
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, output);
  }
}
