/** Test tooling may create roles/templates on its server. A loopback address alone is not
 * evidence that a cluster is disposable: require an explicit acknowledgement as well. */
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const requireDisposableDatabase = (environment) => {
  if (environment.NORBELYS_TEST_DATABASE_DISPOSABLE !== "1") {
    throw new Error(
      "Set NORBELYS_TEST_DATABASE_DISPOSABLE=1 only for a dedicated disposable test cluster."
    );
  }
  let target;
  try {
    target = new URL(environment.TEST_DATABASE_URL);
  } catch {
    throw new Error(
      "Set TEST_DATABASE_URL to the explicitly prepared local test database."
    );
  }
  if (
    !["postgres:", "postgresql:"].includes(target.protocol) ||
    !["localhost", "127.0.0.1", "[::1]"].includes(target.hostname) ||
    !/^\/[a-z_][a-z0-9_]{0,62}$/u.test(target.pathname) ||
    ["/postgres", "/template0", "/template1"].includes(target.pathname) ||
    target.hash ||
    [...target.searchParams.keys()].some((key) => key !== "sslmode")
  ) {
    throw new Error(
      "Tests require a local disposable database, without alternate host/service parameters or reserved database names."
    );
  }
};

if (import.meta.main) {
  requireDisposableDatabase(process.env);
  // Check history before SQLx prepares queries. This never installs pending migrations.
  const commands = [
    ["cargo", ["xtask", "migrate", "--check"]],
    [
      "bun",
      ["x", "turbo", "run", "test-database", "sqlx-check", "--concurrency=1"],
    ],
  ];
  for (const [command, args] of commands) {
    const result = spawnSync(command, args, {
      cwd: fileURLToPath(new URL("../", import.meta.url)),
      env: {
        ...process.env,
        DATABASE_URL: process.env.TEST_DATABASE_URL,
        MIGRATION_DATABASE_URL: process.env.TEST_DATABASE_URL,
      },
      stdio: "inherit",
    });
    if (result.error) {
      throw result.error;
    }
    if (result.status !== 0) {
      process.exitCode = result.status ?? 1;
      break;
    }
  }
}
