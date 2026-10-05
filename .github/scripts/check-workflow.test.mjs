import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { requireDisposableDatabase } from "../../scripts/test-database.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
const environment = {
  TEST_DATABASE_URL: "postgres://fixture@127.0.0.1/test_fixture",
  NORBELYS_TEST_DATABASE_DISPOSABLE: "1",
};

test("explicit test targets require a disposable cluster acknowledgement and cannot override the host", () => {
  expect(() => requireDisposableDatabase(environment)).not.toThrow();
  expect(() =>
    requireDisposableDatabase({
      ...environment,
      NORBELYS_TEST_DATABASE_DISPOSABLE: "",
    })
  ).toThrow();
  for (const target of [
    "postgres://fixture@db.example.com/test_fixture",
    "postgres://fixture@localhost/postgres",
    "postgres://fixture@localhost/test_fixture?host=remote",
    "postgres://fixture@localhost/test_fixture?hostaddr=10.0.0.1",
    "not-a-url",
  ]) {
    expect(() =>
      requireDisposableDatabase({ ...environment, TEST_DATABASE_URL: target })
    ).toThrow();
  }
});

test("the default graph excludes builds, coverage and database lifecycle; mutable database tests are uncached", () => {
  const { tasks } = JSON.parse(
    readFileSync(path.join(root, "turbo.json"), "utf-8")
  );
  const seen = new Set();
  const visit = (name) => {
    if (seen.has(name)) {
      return;
    }
    seen.add(name);
    const task = tasks[name];
    for (const dependency of task?.dependsOn ?? []) {
      // SDK typechecks have no workspace dependency to build; upstream builds are still
      // rejected if a future dependency is added, by the resolved Turbo graph acceptance.
      if (dependency.startsWith("^")) {
        continue;
      }
      visit(
        dependency.includes("#")
          ? dependency
          : `${name.split("#")[0]}#${dependency}`
      );
    }
  };
  visit("//#check:fast");
  expect(
    [...seen].some((name) =>
      /#(?:build|coverage|db-reset|migrate|test-database|sqlx-check)$/u.test(
        name
      )
    )
  ).toBe(false);
  expect(tasks["norbelys-rust#test-database"].cache).toBe(false);
  expect(tasks["norbelys-rust#test-database"].dependsOn ?? []).not.toContain(
    "norbelys-rust#migrate"
  );
});

test("all-tests rejects implicit targets and propagates a suite failure while running the remaining suites", () => {
  const directory = mkdtempSync(
    path.join(tmpdir(), "norbelys-test-orchestration-")
  );
  const log = path.join(directory, "commands");
  try {
    for (const command of ["bun", "cargo"]) {
      writeFileSync(
        path.join(directory, command),
        '#!/bin/sh\nprintf "%s\\n" "$*" >> "$NORBELYS_TEST_COMMAND_LOG"\ncase "$*" in *--workspace*) exit 17;; esac\n',
        { mode: 0o700 }
      );
    }
    const env = {
      ...process.env,
      ...environment,
      PATH: directory,
      NORBELYS_TEST_COMMAND_LOG: log,
    };
    const script = path.join(root, "scripts/test-all.mjs");
    const denied = spawnSync(process.execPath, [script], {
      env: { ...env, NORBELYS_TEST_DATABASE_DISPOSABLE: "" },
    });
    expect(denied.status).not.toBe(0);
    const result = spawnSync(process.execPath, [script], { env });
    expect(result.status).toBe(1);
    const commands = readFileSync(log, "utf-8").trim().split("\n");
    expect(commands).toHaveLength(4);
    expect(
      commands.filter((command) => command.includes("--workspace"))
    ).toHaveLength(1);
    expect(commands.at(-1)).toBe("xtask gates all");
    expect(
      commands.some((command) => /docker|coverage|reset/u.test(command))
    ).toBe(false);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("database checks use the explicit test target for SQLx and propagate failures without resetting it", () => {
  const directory = mkdtempSync(
    path.join(tmpdir(), "norbelys-database-check-")
  );
  const log = path.join(directory, "command");
  try {
    writeFileSync(
      path.join(directory, "cargo"),
      // eslint-disable-next-line no-template-curly-in-string -- Shell expansion in the command fixture.
      '#!/bin/sh\nprintf "%s\\n" "$MIGRATION_DATABASE_URL" "$*" > "$NORBELYS_TEST_COMMAND_LOG"\nexit "${SCHEMA_CHECK_STATUS:-0}"\n',
      { mode: 0o700 }
    );
    writeFileSync(
      path.join(directory, "bun"),
      '#!/bin/sh\nprintf "%s\\n" "$DATABASE_URL" "$*" >> "$NORBELYS_TEST_COMMAND_LOG"\nexit 19\n',
      { mode: 0o700 }
    );
    const result = spawnSync(
      process.execPath,
      [path.join(root, "scripts/test-database.mjs")],
      {
        env: {
          ...process.env,
          ...environment,
          DATABASE_URL: "postgres://fixture@localhost/wrong_target",
          PATH: directory,
          NORBELYS_TEST_COMMAND_LOG: log,
        },
      }
    );
    expect(result.status).toBe(19);
    const [checked, probe, target, command] = readFileSync(log, "utf-8")
      .trim()
      .split("\n");
    expect(checked).toBe(environment.TEST_DATABASE_URL);
    expect(probe).toBe("xtask migrate --check");
    expect(target).toBe(environment.TEST_DATABASE_URL);
    expect(command).toBe(
      "x turbo run test-database sqlx-check --concurrency=1"
    );
    const pending = spawnSync(
      process.execPath,
      [path.join(root, "scripts/test-database.mjs")],
      {
        env: {
          ...process.env,
          ...environment,
          PATH: directory,
          NORBELYS_TEST_COMMAND_LOG: log,
          SCHEMA_CHECK_STATUS: "12",
        },
      }
    );
    expect(pending.status).toBe(12);
    expect(readFileSync(log, "utf-8").trim().split("\n")).toEqual([
      environment.TEST_DATABASE_URL,
      "xtask migrate --check",
    ]);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
