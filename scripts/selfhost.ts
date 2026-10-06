import { spawnSync } from "node:child_process";
/** Prepare a dedicated local Compose installation, run explicit external maintenance, or
 * start compatible runtime images. Private settings are preserved with mode 0600; migrations
 * never execute inside the application container or implicitly during startup.
 */
import { randomBytes } from "node:crypto";
import { chmod, open, readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const path = `${root}.env.selfhost`;

/** Validate a pulled OCI image and retain its immutable digest and source revision. */
export const imageIdentity = (
  reference: string,
  metadata: unknown
): { digest: string; revision: string } => {
  const entries: unknown[] = Array.isArray(metadata) ? metadata : [];
  if (entries.length !== 1) {
    throw new Error("Invalid image metadata.");
  }
  const [entry] = entries;
  if (
    typeof entry !== "object" ||
    entry === null ||
    !("RepoDigests" in entry) ||
    !("Config" in entry)
  ) {
    throw new Error("Invalid image metadata.");
  }
  const digests = entry.RepoDigests;
  const config = entry.Config;
  if (
    !Array.isArray(digests) ||
    typeof config !== "object" ||
    config === null ||
    !("Labels" in config)
  ) {
    throw new Error("Image has no immutable identity.");
  }
  const labels = config.Labels;
  if (
    typeof labels !== "object" ||
    labels === null ||
    !("org.opencontainers.image.revision" in labels)
  ) {
    throw new Error("Image has no source revision.");
  }
  const revision = labels["org.opencontainers.image.revision"];
  const repository = reference.split("@")[0]?.replace(/:[^/]+$/u, "");
  const candidates: unknown[] = digests;
  const digest = candidates.find(
    (candidate): candidate is string =>
      typeof candidate === "string" &&
      candidate.startsWith(`${repository}@sha256:`) &&
      /@sha256:[0-9a-f]{64}$/u.test(candidate) &&
      (!reference.includes("@") || candidate === reference)
  );
  if (
    !digest ||
    typeof revision !== "string" ||
    !/^[0-9a-f]{40}$/u.test(revision)
  ) {
    throw new Error("Image lacks a verified digest/source revision.");
  }
  return { digest, revision };
};

const pinImage = (reference: string) => {
  if (!/^[a-z0-9][a-z0-9./:_@-]+$/u.test(reference)) {
    throw new Error("Invalid image reference.");
  }
  if (
    spawnSync("docker", ["pull", reference], { stdio: "inherit" }).status !== 0
  ) {
    throw new Error("Cannot pull the selected image; settings were preserved.");
  }
  const inspected = spawnSync("docker", ["image", "inspect", reference], {
    encoding: "utf-8",
  });
  if (inspected.status !== 0) {
    throw new Error("Cannot inspect the selected image.");
  }
  const metadata: unknown = JSON.parse(inspected.stdout);
  return imageIdentity(reference, metadata);
};

/** A component can retain its image when CI's inputs for it have not changed. */
export const compatibleImages = async (
  server: string,
  app: string,
  checkout = root
): Promise<boolean> => {
  if (![server, app].every((revision) => /^[0-9a-f]{40}$/u.test(revision))) {
    return false;
  }
  if (server === app) {
    return true;
  }
  const ancestor = (older: string, newer: string) =>
    spawnSync("git", ["merge-base", "--is-ancestor", older, newer], {
      cwd: checkout,
      stdio: "ignore",
    }).status === 0;
  const retained = ancestor(app, server)
    ? "app"
    : ancestor(server, app)
      ? "server"
      : null;
  if (!retained) {
    return false;
  }
  const changed = spawnSync(
    "git",
    ["diff", "--name-only", "--no-renames", "-z", server, app],
    { cwd: checkout, encoding: "utf-8" }
  );
  if (changed.status !== 0) {
    return false;
  }
  const module: unknown = await import(
    new URL("../.github/scripts/release-scope.mjs", import.meta.url).href
  );
  const publicationRules = (
    value: unknown
  ): value is { scope: (paths: string[]) => unknown } =>
    typeof value === "object" &&
    value !== null &&
    "scope" in value &&
    typeof value.scope === "function";
  if (!publicationRules(module)) {
    throw new Error("The installation needs its image publication rules.");
  }
  const selection = module.scope(changed.stdout.split("\0").filter(Boolean));
  const images: unknown[] = Array.isArray(selection) ? selection : [];
  if (
    !Array.isArray(selection) ||
    !images.every((image): image is string => typeof image === "string")
  ) {
    throw new Error("Invalid image publication selection.");
  }
  return !images.includes(retained);
};

const run = (
  args: string[],
  input?: string,
  environment: Record<string, string> = {}
) => {
  const plugin = spawnSync("docker", ["compose", "version"], {
    stdio: "ignore",
  });
  const executable = plugin.status === 0 ? "docker" : "docker-compose";
  const compose = [
    ...(plugin.status === 0 ? ["compose"] : []),
    "--env-file",
    path,
    "-f",
    `${root}compose.yml`,
  ];
  const result = spawnSync(executable, [...compose, ...args], {
    cwd: root,
    env: { ...process.env, ...environment },
    input,
    stdio: input ? ["pipe", "inherit", "inherit"] : "inherit",
  });
  if (result.status !== 0) {
    throw new Error(
      "Compose failed; private settings and volumes were preserved."
    );
  }
};
/** Explicit maintenance uses the exact source that produced the pinned runtime images. */
const migrate = (values: Record<string, string>) => {
  const revision = values.SELFHOST_REVISION;
  if (!revision || !/^[0-9a-f]{40}$/u.test(revision)) {
    throw new Error("Run selfhost:setup to select the runtime images first.");
  }
  const head = spawnSync("git", ["rev-parse", "HEAD"], {
    cwd: root,
    encoding: "utf-8",
  });
  const paths = [
    "Cargo.toml",
    "Cargo.lock",
    "tools/xtask",
    "crates/server/migrations",
    "crates/server/provision.sql",
    "crates/server/schema-check.sql",
  ];
  const changes = spawnSync("git", ["status", "--porcelain", "--", ...paths], {
    cwd: root,
    encoding: "utf-8",
  });
  if (
    head.status !== 0 ||
    head.stdout.trim() !== revision ||
    changes.status !== 0 ||
    changes.stdout.trim()
  ) {
    throw new Error(
      "External maintenance requires the clean source checkout matching SELFHOST_REVISION."
    );
  }
  const port = values.DATABASE_PORT ?? "5432";
  if (
    !/^[0-9]{1,5}$/u.test(port) ||
    Number(port) < 1 ||
    Number(port) > 65_535
  ) {
    throw new Error("DATABASE_PORT must be a valid local port.");
  }
  for (const name of [
    "POSTGRES_PASSWORD",
    "APP_DATABASE_PASSWORD",
    "WORKER_DATABASE_PASSWORD",
    "TRACKING_DATABASE_PASSWORD",
    "SYSTEM_DATABASE_PASSWORD",
  ]) {
    if (!/^[0-9a-f]{48}$/u.test(values[name] ?? "")) {
      throw new Error(
        "Invalid local database password; private settings were preserved."
      );
    }
  }
  const url = new URL(`postgres://norbelys@127.0.0.1:${port}/norbelys`);
  url.password = values.POSTGRES_PASSWORD ?? "";
  const env = {
    ...process.env,
    ...values,
    MIGRATION_DATABASE_URL: url.href,
    SQLX_OFFLINE: "true",
  };
  for (const command of ["provision", "migrate", "migrate --check"]) {
    const result = spawnSync(
      "cargo",
      ["run", "--locked", "-p", "xtask", "--", ...command.split(" ")],
      { cwd: root, env, stdio: "inherit" }
    );
    if (result.error || result.status !== 0) {
      throw new Error(
        "External maintenance failed; runtime services were not restarted."
      );
    }
  }
  // Installation keys are data, independent of versioned schema migrations. The one-off
  // maintenance process uses the system login; the API keeps its restricted runtime login.
  const system = new URL("postgres://norbelys_system@postgres:5432/norbelys");
  system.password = values.SYSTEM_DATABASE_PASSWORD ?? "";
  run(
    [
      "run",
      "--rm",
      "--no-deps",
      "-T",
      "--env",
      "DATABASE_URL",
      "api",
      "/app/norbelys-server",
      "admin",
      "keys",
      "ensure",
    ],
    undefined,
    { DATABASE_URL: system.href }
  );
  console.log(
    "Schema and signing key prepared. Run bun run selfhost:start to verify and start the pinned images."
  );
};

if (import.meta.main) {
  process.umask(0o077);
  const args = process.argv.slice(2);
  if (
    args.length > 1 ||
    args.some((arg) => !["--migrate", "--start", "--upgrade"].includes(arg))
  ) {
    throw new Error(
      "Choose setup, --upgrade, --migrate, or --start separately."
    );
  }
  let settings: string;
  try {
    settings = await readFile(path, "utf-8");
  } catch (error) {
    if (
      !(error instanceof Error && "code" in error && error.code === "ENOENT")
    ) {
      throw error;
    }
    if (args.length > 0) {
      throw new Error(
        "Run selfhost:setup before maintenance, startup, or upgrades.",
        { cause: error }
      );
    }
    const key = randomBytes(32).toString("base64");
    settings = `NORBELYS_DEPLOYMENT_KEY=${key}\n`;
    for (const name of [
      "POSTGRES_PASSWORD",
      "APP_DATABASE_PASSWORD",
      "WORKER_DATABASE_PASSWORD",
      "TRACKING_DATABASE_PASSWORD",
      "SYSTEM_DATABASE_PASSWORD",
    ]) {
      settings += `${name}=${randomBytes(24).toString("hex")}\n`;
    }
    const handle = await open(path, "wx", 0o600);
    try {
      await handle.writeFile(settings);
    } finally {
      await handle.close();
    }
  }
  await chmod(path, 0o600);
  const values = Object.fromEntries(
    settings
      .trim()
      .split("\n")
      .map((line) => {
        const separator = line.indexOf("=");
        return [line.slice(0, separator), line.slice(separator + 1)];
      })
  );
  if (process.argv.includes("--migrate")) {
    migrate(values);
  } else if (process.argv.includes("--start")) {
    if (
      !/^[0-9a-f]{40}$/u.test(values.SELFHOST_REVISION ?? "") ||
      ![values.SERVER_IMAGE, values.APP_IMAGE].every((image) =>
        /^[a-z0-9][a-z0-9./:_-]+@sha256:[0-9a-f]{64}$/u.test(image ?? "")
      )
    ) {
      throw new Error("Run selfhost:setup to pin both runtime images first.");
    }
    run([
      "run",
      "--rm",
      "--no-deps",
      "-T",
      "api",
      "/app/norbelys-server",
      "admin",
      "schema-check",
    ]);
    run(["up", "-d", "--wait"]);
    console.log(
      "Local installation ready at http://localhost:5173. Private settings remain in .env.selfhost."
    );
  } else {
    const upgrade = process.argv.includes("--upgrade");
    if (
      upgrade &&
      (!process.env.NORBELYS_SERVER_IMAGE || !process.env.NORBELYS_APP_IMAGE)
    ) {
      throw new Error(
        "An upgrade requires compatible NORBELYS_SERVER_IMAGE and NORBELYS_APP_IMAGE references."
      );
    }
    if (
      upgrade ||
      !/^SERVER_IMAGE=.+@sha256:[0-9a-f]{64}$/mu.test(settings) ||
      !/^APP_IMAGE=.+@sha256:[0-9a-f]{64}$/mu.test(settings)
    ) {
      const server = pinImage(
        process.env.NORBELYS_SERVER_IMAGE ??
          "ghcr.io/eusoumaxi/norbelys-server:latest"
      );
      const app = pinImage(
        process.env.NORBELYS_APP_IMAGE ??
          "ghcr.io/eusoumaxi/norbelys-app:latest"
      );
      if (!(await compatibleImages(server.revision, app.revision))) {
        throw new Error(
          "The selected images have divergent or changed component inputs; select a compatible pair explicitly."
        );
      }
      settings = settings
        .replaceAll(
          /^(?:SERVER_IMAGE|APP_IMAGE|SELFHOST_REVISION|APP_REVISION)=.*\n?/gmu,
          ""
        )
        .trimEnd();
      settings += `\nSERVER_IMAGE=${server.digest}\nAPP_IMAGE=${app.digest}\nSELFHOST_REVISION=${server.revision}\nAPP_REVISION=${app.revision}\n`;
      await writeFile(path, settings, { mode: 0o600 });
    }
    run(["up", "-d", "--wait", "postgres"]);
    console.log(
      "Settings and PostgreSQL are ready. Use the clean checkout matching SELFHOST_REVISION to run bun run selfhost:migrate, then bun run selfhost:start. Existing runtime services have not been restarted."
    );
  }
}
