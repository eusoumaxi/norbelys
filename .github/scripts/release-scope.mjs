// Which images a push to main rebuilds, from the files it changed (the Images workflow):
//
// - server (ghcr.io/<owner>/norbelys-server): the server crate and the libraries it links, the committed
//   query metadata, and every shared build input;
// - server-analytics: the same server binary with the analytics feature, so every input that
//   rebuilds server also rebuilds this image (including configuration and linked libraries);
// - smtp: the managed MTA's crate and the mail library it links, and every shared build input;
// - collector: its Dockerfile, component manifest and configuration.
//
// Cargo build inputs rebuild the three Rust images. Publication orchestration changes rebuild
// every image. A manual run, or a missing successful publication baseline, also builds all images.
// All files under linked crates participate, including embedded text assets.
import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";

import { successfulBase } from "./successful-base.mjs";

export const IMAGES = [
  "server",
  "server-analytics",
  "smtp",
  "collector",
  "app",
];

const RUST_SHARED = new Set([
  "Cargo.toml",
  "Cargo.lock",
  "rust-toolchain.toml",
  "Dockerfile",
  ".dockerignore",
]);
const PUBLICATION_SHARED = new Set([
  ".github/scripts/release-scope.mjs",
  ".github/scripts/successful-base.mjs",
  ".github/workflows/images.yml",
]);

const SERVER_INPUTS = [
  ".env.example",
  "crates/server/",
  "crates/mail/",
  "crates/ai/",
  ".sqlx/",
];

const OWNERS = {
  server: SERVER_INPUTS,
  "server-analytics": SERVER_INPUTS,
  smtp: ["crates/smtp/", "crates/mail/"],
  collector: ["docker/collector/"],
  app: [
    "apps/app/",
    "brand/",
    "sdks/typescript/",
    "package.json",
    "bun.lock",
    "patches/",
    "apps/web/package.json",
    "apps/cli/package.json",
    "apps/docs/package.json",
    "crates/server/openapi.json",
  ],
};

/** The images to rebuild for a list of changed paths. */
export const scope = (paths) => {
  const selected = new Set();
  for (const path of paths) {
    if (RUST_SHARED.has(path) || PUBLICATION_SHARED.has(path)) {
      const images = PUBLICATION_SHARED.has(path)
        ? IMAGES
        : ["server", "server-analytics", "smtp"];
      for (const image of images) {
        selected.add(image);
      }
    }
    for (const [image, prefixes] of Object.entries(OWNERS)) {
      if (
        prefixes.some((prefix) => path === prefix || path.startsWith(prefix))
      ) {
        selected.add(image);
      }
    }
  }
  return IMAGES.filter((image) => selected.has(image));
};

/** Accept only the finite image selection produced by CI for this exact source. */
export const verifiedImages = (plan, sha) => {
  if (
    !/^[0-9a-f]{40}$/u.test(sha ?? "") ||
    plan?.sha !== sha ||
    !Array.isArray(plan.images) ||
    plan.images.length > IMAGES.length ||
    new Set(plan.images).size !== plan.images.length ||
    plan.images.some((image) => !IMAGES.includes(image))
  ) {
    throw new Error("Image selection does not match the successful CI source.");
  }
  return IMAGES.filter((image) => plan.images.includes(image));
};

/** Include changes from failed or cancelled publications; no baseline builds everything. */
const changedPaths = () => {
  if (process.env.GITHUB_EVENT_NAME !== "push") {
    return null;
  }
  const base = successfulBase("images.yml", process.env.GITHUB_SHA);
  if (!base) {
    return null;
  }
  const output = execFileSync(
    "git",
    ["diff", "--name-only", "--no-renames", "-z", base, process.env.GITHUB_SHA],
    { encoding: "utf-8" }
  );
  return output.split("\0").filter(Boolean);
};

if (import.meta.main) {
  let images;
  if (process.env.GITHUB_EVENT_NAME === "workflow_run") {
    images = verifiedImages(
      JSON.parse(readFileSync(process.env.RELEASE_SCOPE_PATH, "utf-8")),
      process.env.SOURCE_SHA
    );
  } else {
    const paths = changedPaths();
    images = paths === null ? IMAGES : scope(paths);
    if (process.env.RELEASE_SCOPE_PATH) {
      writeFileSync(
        process.env.RELEASE_SCOPE_PATH,
        JSON.stringify({
          sha: process.env.GITHUB_SHA,
          images,
        })
      );
    }
  }
  const lines = `images=${JSON.stringify(images)}\nany=${images.length > 0}\n`;
  process.stdout.write(lines);
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, lines);
  }
}
