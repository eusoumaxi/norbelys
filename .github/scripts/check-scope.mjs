/** Select checks from both sides of a rename/deletion; an unknown baseline runs every suite. */
import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";

import { successfulBase } from "./successful-base.mjs";

const affectsBackend = (path) =>
  ["crates/", "tools/xtask/", ".cargo/", ".sqlx/", "docker/"].some((prefix) =>
    path.startsWith(prefix)
  ) ||
  [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "deny.toml",
    "rustfmt.toml",
    "clippy.toml",
    "dist-workspace.toml",
    ".env.example",
    "Dockerfile",
    "scripts/rust-coverage.mjs",
    "scripts/test-all.mjs",
    "scripts/test-database.mjs",
    "scripts/selfhost.ts",
    "scripts/dev-init.sh",
    "docker/coverage.yml",
  ].includes(path);

export const selectChecks = (paths) => {
  const result = {
    backend: false,
    typescript: false,
    python: false,
    frontends: [],
  };
  const apps = new Set();
  let cliOnly = true;
  const all = () => {
    result.backend = true;
    cliOnly = false;
    result.typescript = true;
    result.python = true;
    for (const app of ["app", "web", "cli-downloads"]) {
      apps.add(app);
    }
  };
  if (paths === null) {
    all();
  }
  for (const path of paths ?? []) {
    if (
      [
        "package.json",
        "bun.lock",
        "turbo.json",
        "pyproject.toml",
        ".python-version",
        ".editorconfig",
        "uv.lock",
      ].includes(path) ||
      path.startsWith(".github/") ||
      path.startsWith("patches/") ||
      path.startsWith("tools/codegen/")
    ) {
      all();
    }
    if (affectsBackend(path)) {
      result.backend = true;
      if (!path.startsWith("crates/cli/")) {
        cliOnly = false;
      }
    }
    if (path.startsWith("sdks/typescript/")) {
      result.typescript = true;
      apps.add("app");
    }
    if (path.startsWith("sdks/python/")) {
      result.python = true;
    }
    if (path === "crates/server/openapi.json") {
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
      apps.add("app");
      apps.add("web");
    }
  }
  result.frontends = [...apps].toSorted();
  result.backend_mode = result.backend && cliOnly ? "cli" : "workspace";
  return result;
};
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
  const selected = selectChecks(changedPaths(base, head));
  const output = Object.entries(selected)
    .map(([key, value]) => `${key}=${JSON.stringify(value)}\n`)
    .join("");
  process.stdout.write(output);
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, output);
  }
}
