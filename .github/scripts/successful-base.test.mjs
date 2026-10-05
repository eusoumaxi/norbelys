import { expect, test } from "bun:test";

import { selectSuccessfulBase } from "./successful-base.mjs";

const repository = "owner/product";
const current = "c".repeat(40);
const earlier = "a".repeat(40);
const run = {
  conclusion: "success",
  event: "push",
  head_branch: "main",
  head_repository: { full_name: repository },
  head_sha: earlier,
};

test("failed and cancelled commits remain in the next successful comparison", () => {
  const runs = [
    { ...run, head_sha: current },
    { ...run, head_sha: "b".repeat(40), conclusion: "failure" },
    { ...run, head_sha: "d".repeat(40), conclusion: "cancelled" },
    run,
  ];
  expect(
    selectSuccessfulBase(runs, repository, current, "ci.yml", () => true)
  ).toBe(earlier);
});

test("forks, pull requests and unrelated history cannot establish a baseline", () => {
  const runs = [
    { ...run, head_repository: { full_name: "fork/product" } },
    { ...run, event: "pull_request" },
    { ...run, head_branch: "feature" },
    run,
  ];
  expect(
    selectSuccessfulBase(runs, repository, current, "ci.yml", () => false)
  ).toBeNull();
});

test("image history uses the published source instead of the workflow definition", () => {
  expect(
    selectSuccessfulBase(
      [
        { ...run, event: "workflow_run", display_title: "Images" },
        {
          ...run,
          event: "workflow_run",
          head_sha: current,
          display_title: `Images ${earlier}`,
        },
      ],
      repository,
      current,
      "images.yml",
      () => true
    )
  ).toBe(earlier);
});

test("no successful publication rebuilds every image through a null baseline", () => {
  expect(
    selectSuccessfulBase([], repository, current, "images.yml", () => true)
  ).toBeNull();
});
