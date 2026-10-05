/** Report native coverage on a disposable database; compilation and test failures still fail.
 * The percentage is informational while the functional refactor is being completed.
 */
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { requireDisposableDatabase } from "./test-database.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
const [scope] = process.argv.slice(2);
if (scope !== "workspace" && scope !== "cli") {
  throw new Error("Coverage scope must be workspace or cli.");
}
mkdirSync(`${root}/coverage`, { recursive: true });
const environment = { ...process.env };
let compose;
try {
  if (scope === "workspace" && !environment.TEST_DATABASE_URL) {
    const plugin = spawnSync("docker", ["compose", "version"], {
      stdio: "ignore",
    });
    const executable = plugin.status === 0 ? "docker" : "docker-compose";
    const prefix = plugin.status === 0 ? ["compose"] : [];
    const project = `norbelys-coverage-${process.pid}`;
    compose = (...args) =>
      execFileSync(
        executable,
        [...prefix, "-p", project, "-f", "docker/coverage.yml", ...args],
        { cwd: root, encoding: "utf-8", stdio: ["ignore", "pipe", "inherit"] }
      );
    compose("up", "--detach", "--wait", "postgres");
    const address = compose("port", "postgres", "5432").trim();
    environment.TEST_DATABASE_URL = `postgres://norbelys:coverage-only@${address}/norbelys`;
    environment.NORBELYS_TEST_DATABASE_DISPOSABLE = "1";
  }
  if (scope === "workspace") {
    requireDisposableDatabase(environment);
  }
  const result = spawnSync(
    "cargo",
    [
      "llvm-cov",
      "--locked",
      ...(scope === "cli"
        ? ["-p", "norbelys-cli"]
        : ["--workspace", "--all-features"]),
      "--json",
      "--output-path",
      `coverage/${scope === "cli" ? "cli-rust" : "rust"}.json`,
      "--show-missing-lines",
    ],
    { cwd: root, env: environment, stdio: "inherit" }
  );
  if (result.error) {
    throw result.error;
  }
  process.exitCode = result.status ?? 1;
} finally {
  if (compose) {
    compose("down");
  }
}
