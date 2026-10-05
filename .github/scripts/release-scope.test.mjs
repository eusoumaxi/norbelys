// The decisions of release-scope.mjs, run by the Images workflow before it decides
// (`bun test ./.github/scripts/release-scope.test.mjs`): an image missed means a release that
// ships an old binary.
import { expect, test } from "bun:test";

import { IMAGES, scope, verifiedImages } from "./release-scope.mjs";

test("both server images include shared runtime and query changes", () => {
  for (const path of [
    ".env.example",
    "crates/server/src/people.rs",
    "crates/server/src/config.rs",
    "crates/server/src/storage.rs",
    "crates/server/src/telemetry.rs",
    "crates/ai/src/client.rs",
    ".sqlx/query-1.json",
  ]) {
    expect(scope([path])).toEqual(["server", "server-analytics"]);
  }
});

test("the mail library is linked by the server and the MTA", () => {
  expect(scope(["crates/mail/src/dsn.rs"])).toEqual([
    "server",
    "server-analytics",
    "smtp",
  ]);
});

test("collector changes rebuild its independent image", () => {
  expect(scope(["docker/collector/builder.yaml"])).toEqual(["collector"]);
  expect(scope(["docker/collector/Dockerfile"])).toEqual(["collector"]);
});

test("analytics paths and the schema rebuild the analytics image too", () => {
  expect(scope(["crates/server/src/roles/analytics.rs"])).toEqual([
    "server",
    "server-analytics",
  ]);
  expect(scope(["crates/server/migrations/0001_initial.sql"])).toEqual([
    "server",
    "server-analytics",
  ]);
});

test("the MTA's crate rebuilds its image only", () => {
  expect(scope(["crates/smtp/src/tail.rs"])).toEqual(["smtp"]);
});

test("shared build inputs rebuild every image", () => {
  for (const path of ["Cargo.lock", "Dockerfile", "rust-toolchain.toml"]) {
    expect(scope([path])).toEqual(IMAGES.filter((image) => image !== "app"));
  }
});

test("publication workflow and scope changes rebuild the dashboard image too", () => {
  expect(scope([".github/workflows/images.yml"])).toEqual(IMAGES);
  expect(scope([".github/scripts/successful-base.mjs"])).toEqual(IMAGES);
});

test("the CLI, the apps and private deployment templates rebuild nothing", () => {
  expect(scope(["crates/cli/src/main.rs", "deploy/compose/core.yml"])).toEqual(
    []
  );
});

test("embedded Markdown in linked crates participates in releases", () => {
  expect(scope(["crates/ai/prompts/system.md"])).toEqual([
    "server",
    "server-analytics",
  ]);
});

test("dashboard source and shared brand select its own container", () => {
  expect(scope(["apps/app/src/main.tsx"])).toEqual(["app"]);
  expect(scope(["brand/icons/favicon.svg"])).toEqual(["app"]);
  expect(scope(["apps/docs/package.json"])).toEqual(["app"]);
  expect(scope(["crates/server/openapi.json"])).toEqual([
    "server",
    "server-analytics",
    "app",
  ]);
});

test("image publication accepts only the finite selection for the successful CI commit", () => {
  const sha = "a".repeat(40);
  expect(verifiedImages({ sha, images: ["app", "server"] }, sha)).toEqual([
    "server",
    "app",
  ]);
  expect(verifiedImages({ sha, images: [] }, sha)).toEqual([]);
  for (const plan of [
    null,
    {},
    { sha, images: ["shell"] },
    { sha, images: ["app", "app"] },
    { sha: "b".repeat(40), images: ["app"] },
  ]) {
    expect(() => verifiedImages(plan, sha)).toThrow();
  }
  expect(() => verifiedImages({ sha: "main", images: [] }, "main")).toThrow();
});
