/** Audit current advisories on every CI run; task-result caches must never cache this gate. */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

// The registry still reports the original versions. Accept these two advisories only
// when Bun's manifest and lockfile bind the exact reviewed remediation bytes.
// Updating a package or patch requires a new review; there is no time-based waiver.
export const remediations = [
  {
    package: "braces",
    advisory: "GHSA-vfj7-8cjw-p6xm",
    version: "3.0.3",
    patch: "patches/braces@3.0.3.patch",
    sha256: "a393267dd71183fcc52c53da4de963cdc35d869f2022a4214b6b1b3fe7ee9a44",
    reason: "Bound parser nesting and direct recursive AST traversals.",
  },
  {
    package: "http-cache-semantics",
    advisory: "GHSA-ch52-4w7c-c8xp",
    version: "4.2.0",
    patch: "patches/http-cache-semantics@4.2.0.patch",
    sha256: "99884ce9c65bfd50d13d8d55a4d7f0ae04e8bb3d7b6cdc19068da99a587174b4",
    reason:
      "Require revalidation for zero-lifetime responses on all stale-serving paths.",
  },
];

export const verifiedRemediations = (
  manifest,
  lock,
  readPatch = readFileSync
) => {
  if (!lock?.packages || typeof lock.packages !== "object") {
    throw new TypeError("Invalid dependency lockfile");
  }
  const verified = new Set();
  for (const item of remediations) {
    const key = `${item.package}@${item.version}`;
    const packages = Object.values(lock.packages).filter(
      (entry) =>
        Array.isArray(entry) && entry[0]?.startsWith(`${item.package}@`)
    );
    if (packages.length === 0) {
      continue;
    }
    if (
      packages.some((entry) => entry[0] !== key) ||
      manifest.patchedDependencies?.[key] !== item.patch ||
      lock.patchedDependencies?.[key] !== item.patch ||
      createHash("sha256").update(readPatch(item.patch)).digest("hex") !==
        item.sha256
    ) {
      throw new Error(`Dependency remediation must be reviewed: ${key}`);
    }
    verified.add(item.advisory);
  }
  return verified;
};

/** Unknown advisories, missing patches and malformed reports fail closed at every severity. */
export const auditFailures = (report, verified = new Set()) => {
  if (!report || typeof report !== "object" || Array.isArray(report)) {
    throw new TypeError("Invalid dependency audit report");
  }
  const failures = [];
  for (const [name, advisories] of Object.entries(report)) {
    if (!Array.isArray(advisories)) {
      throw new TypeError("Invalid dependency audit entries");
    }
    for (const advisory of advisories) {
      if (!advisory || typeof advisory.url !== "string") {
        throw new TypeError("Invalid dependency advisory");
      }
      const id =
        /^https:\/\/github\.com\/advisories\/(?<id>GHSA-[a-z0-9-]+)$/u.exec(
          advisory.url
        )?.groups.id;
      if (
        !verified.has(id) ||
        !remediations.some(
          (item) => item.package === name && item.advisory === id
        )
      ) {
        failures.push(
          `${name}: ${id ?? "unknown advisory"} (${advisory.severity ?? "unknown severity"})`
        );
      }
    }
  }
  return failures;
};

if (import.meta.main) {
  const manifest = JSON.parse(readFileSync("package.json", "utf-8"));
  const lock = Bun.JSONC.parse(readFileSync("bun.lock", "utf-8"));
  const verified = verifiedRemediations(manifest, lock);
  const result = spawnSync("bun", ["audit", "--json"], { encoding: "utf-8" });
  // Bun returns 1 for advisories; transport errors must still fail closed.
  if (
    result.error ||
    result.signal ||
    ![0, 1].includes(result.status) ||
    !result.stdout.trim()
  ) {
    throw new Error("Dependency audit could not complete", {
      cause: result.error,
    });
  }
  const report = JSON.parse(result.stdout);
  const failures = auditFailures(report, verified);
  if (failures.length > 0) {
    throw new Error(`Dependency security gate failed:\n${failures.join("\n")}`);
  }
  for (const item of remediations) {
    if (
      report[item.package]?.some((entry) => entry.url.endsWith(item.advisory))
    ) {
      process.stdout.write(
        `Verified local remediation: ${item.advisory}: ${item.reason}\n`
      );
    }
  }
}
