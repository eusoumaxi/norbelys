import { execFileSync } from "node:child_process";

/** Pick a verified earlier main push; unrelated histories cannot hide changed files. */
export const selectSuccessfulBase = (
  runs,
  repository,
  head,
  workflow,
  isAncestor
) => {
  for (const run of runs) {
    // workflow_run executes the current workflow definition. Its head_sha can differ
    // from the product source that was built, so image runs name that source explicitly.
    const source =
      workflow === "images.yml"
        ? /^Images ([0-9a-f]{40})$/u.exec(run.display_title ?? "")?.[1]
        : run.head_sha;
    const expectedEvent =
      workflow === "images.yml"
        ? ["workflow_run", "workflow_dispatch"].includes(run.event)
        : run.event === "push";
    if (
      run.conclusion === "success" &&
      expectedEvent &&
      run.head_branch === "main" &&
      run.head_repository?.full_name === repository &&
      /^[0-9a-f]{40}$/u.test(source ?? "") &&
      source !== head &&
      isAncestor(source, head)
    ) {
      return source;
    }
  }
  return null;
};

/** Missing history or API access conservatively selects all checks and images. */
export const successfulBase = (workflow, head) => {
  const repository = process.env.GITHUB_REPOSITORY;
  if (
    !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/u.test(repository ?? "") ||
    !/^[0-9a-f]{40}$/u.test(head ?? "")
  ) {
    return null;
  }
  try {
    const result = JSON.parse(
      execFileSync(
        "gh",
        [
          "api",
          `repos/${repository}/actions/workflows/${workflow}/runs?branch=main&status=success&per_page=100`,
        ],
        { encoding: "utf-8" }
      )
    );
    return selectSuccessfulBase(
      result.workflow_runs,
      repository,
      head,
      workflow,
      (base, source) => {
        try {
          execFileSync("git", ["merge-base", "--is-ancestor", base, source]);
          return true;
        } catch {
          return false;
        }
      }
    );
  } catch {
    console.warn(
      "::warning::No verified successful baseline; selecting all work."
    );
    return null;
  }
};
