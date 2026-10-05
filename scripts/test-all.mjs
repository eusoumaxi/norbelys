/** Deliberate complete test execution, once per suite, without coverage or fixture lifecycle.
 * Prepare a disposable PostgreSQL service first. A failing suite is reported and the remaining
 * suites still run; infrastructure setup errors must be resolved before invoking this command. */
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { requireDisposableDatabase } from "./test-database.mjs";

requireDisposableDatabase(process.env);
const root = fileURLToPath(new URL("../", import.meta.url));
const suites = [
  ["bun", ["test", "./.github/scripts"]],
  [
    "bun",
    [
      "x",
      "turbo",
      "run",
      "test",
      "--filter=@norbelys/sdk",
      "--filter=@norbelys/app",
      "--filter=@norbelys/cli-downloads",
      "--filter=norbelys",
      "--concurrency=1",
    ],
  ],
  ["cargo", ["test", "--locked", "--workspace", "--all-features"]],
  ["cargo", ["xtask", "gates", "all"]],
];
for (const [program, args] of suites) {
  const result = spawnSync(program, args, {
    cwd: root,
    stdio: "inherit",
    env: { ...process.env, SQLX_OFFLINE: "true" },
  });
  if (result.error || result.status !== 0) {
    console.error(
      `${program} ${args.join(" ")} failed (${result.status ?? "could not start"}).`
    );
    process.exitCode = 1;
  }
}
