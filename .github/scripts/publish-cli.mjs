/** Attach native assets to the release-please release; retries preserve published bytes. */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import pathModule from "node:path";

const gh = (...args) =>
  execFileSync("gh", args, {
    encoding: "utf-8",
    stdio: ["ignore", "pipe", "pipe"],
  });
const { RELEASE_TAG: tag, GITHUB_REPOSITORY: repository } = process.env;
if (
  !/^norbelys-cli-v\d+\.\d+\.\d+(?:-[\w.-]+)?$/u.test(tag ?? "") ||
  !/^[\w.-]+\/[\w.-]+$/u.test(repository ?? "")
) {
  throw new Error("A CLI version tag and source repository are required.");
}
let release;
try {
  release = JSON.parse(gh("api", `repos/${repository}/releases/tags/${tag}`));
} catch (error) {
  if (!String(error.stderr).includes("HTTP 404")) {
    throw error;
  }
  const notes = pathModule.join(process.env.RUNNER_TEMP, "notes.txt");
  writeFileSync(notes, process.env.ANNOUNCEMENT_BODY);
  gh(
    "release",
    "create",
    tag,
    "--repo",
    repository,
    "--target",
    process.env.GITHUB_SHA,
    "--title",
    process.env.ANNOUNCEMENT_TITLE,
    "--notes-file",
    notes,
    ...(process.env.PRERELEASE_FLAG ? ["--prerelease"] : [])
  );
  release = JSON.parse(gh("api", `repos/${repository}/releases/tags/${tag}`));
}
for (const name of readdirSync("artifacts")) {
  const path = pathModule.join("artifacts", name);
  const digest = `sha256:${createHash("sha256").update(readFileSync(path)).digest("hex")}`;
  const existing = release.assets.find((asset) => asset.name === name);
  if (existing) {
    if (existing.digest !== digest) {
      throw new Error(
        `Published asset ${name} differs; a new version is required.`
      );
    }
  } else {
    gh("release", "upload", tag, path, "--repo", repository);
  }
}
