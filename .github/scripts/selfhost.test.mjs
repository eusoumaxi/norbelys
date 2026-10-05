import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  existsSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { imageIdentity } from "../../scripts/selfhost.ts";

const repository = "ghcr.io/norbelys/norbelys";
const digest = `${repository}@sha256:${"a".repeat(64)}`;
const revision = "b".repeat(40);
const metadata = () => [
  {
    RepoDigests: [digest],
    Config: { Labels: { "org.opencontainers.image.revision": revision } },
  },
];

test("self-host setup pins the selected repository and its reviewed source", () => {
  expect(imageIdentity(`${repository}:v0.1.0`, metadata())).toEqual({
    digest,
    revision,
  });
  expect(imageIdentity(digest, metadata())).toEqual({ digest, revision });
  expect(() =>
    imageIdentity(`${repository}@sha256:${"c".repeat(64)}`, metadata())
  ).toThrow();
  for (const malformed of [
    null,
    [],
    [{}],
    [...metadata(), ...metadata()],
    [
      {
        RepoDigests: [`ghcr.io/other/product@sha256:${"a".repeat(64)}`],
        Config: metadata()[0].Config,
      },
    ],
    [
      {
        RepoDigests: [digest],
        Config: { Labels: { "org.opencontainers.image.revision": "main" } },
      },
    ],
    [{ RepoDigests: [digest], Config: { Labels: {} } }],
    [{ RepoDigests: null, Config: {} }],
  ]) {
    expect(() => imageIdentity(`${repository}:v0.1.0`, malformed)).toThrow();
  }
});

/** Execute the operator entry point with a disposable checkout and no Docker daemon. */
const runScript = (args, settings, schemaStatus = 0) => {
  const root = mkdtempSync(path.join(tmpdir(), "norbelys-selfhost-flow-"));
  try {
    mkdirSync(path.join(root, "scripts"));
    mkdirSync(path.join(root, "bin"));
    copyFileSync(
      new URL("../../scripts/selfhost.ts", import.meta.url),
      path.join(root, "scripts/selfhost.ts")
    );
    if (settings !== undefined) {
      writeFileSync(path.join(root, ".env.selfhost"), settings, {
        mode: 0o600,
      });
    }
    const calls = path.join(root, "calls");
    writeFileSync(
      path.join(root, "bin/docker"),
      `#!/bin/sh
printf '%s\\n' "$*" >> "$SELFHOST_CALLS"
case "$*" in *schema-check*) exit ${schemaStatus} ;; esac
exit 0
`,
      { mode: 0o700 }
    );
    const result = spawnSync(
      process.execPath,
      [path.join(root, "scripts/selfhost.ts"), ...args],
      {
        cwd: root,
        encoding: "utf-8",
        env: {
          PATH: `${path.join(root, "bin")}:${process.env.PATH}`,
          SELFHOST_CALLS: calls,
        },
      }
    );
    return {
      status: result.status,
      error: result.stderr,
      calls: existsSync(calls) ? readFileSync(calls, "utf-8") : "",
      settings: existsSync(path.join(root, ".env.selfhost")),
    };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
};

test("startup cannot generate credentials or use unpinned runtime images", () => {
  for (const mode of ["--start", "--migrate", "--upgrade"]) {
    const result = runScript([mode]);
    expect(result.status).not.toBe(0);
    expect(result.settings).toBe(false);
    expect(result.calls).toBe("");
  }
  const result = runScript(
    ["--start"],
    "SERVER_IMAGE=ghcr.io/example/server:latest\n"
  );
  expect(result.status).not.toBe(0);
  expect(result.calls).toBe("");
});

test("runtime startup follows a successful read-only schema check", () => {
  const settings = `SERVER_IMAGE=${digest}\nAPP_IMAGE=${digest}\nSELFHOST_REVISION=${revision}\n`;
  const refused = runScript(["--start"], settings, 1);
  expect(refused.status).not.toBe(0);
  expect(refused.calls).toContain("admin schema-check");
  expect(refused.calls).not.toContain("up -d");
  const accepted = runScript(["--start"], settings);
  expect(accepted.status).toBe(0);
  expect(accepted.calls.indexOf("admin schema-check")).toBeLessThan(
    accepted.calls.indexOf("up -d --wait")
  );
  expect(accepted.calls).not.toContain("migrate");
});

test("mixed or unknown maintenance modes fail before touching settings", () => {
  for (const args of [["--start", "--migrate"], ["--unknown"]]) {
    const result = runScript(args);
    expect(result.status).not.toBe(0);
    expect(result.settings).toBe(false);
    expect(result.calls).toBe("");
  }
});
