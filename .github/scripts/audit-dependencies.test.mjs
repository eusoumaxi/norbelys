import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import {
  auditFailures,
  remediations,
  verifiedRemediations,
} from "./audit-dependencies.mjs";

const manifest = JSON.parse(
  readFileSync(new URL("../../package.json", import.meta.url), "utf-8")
);
const lock = Bun.JSONC.parse(
  readFileSync(new URL("../../bun.lock", import.meta.url), "utf-8")
);
const readPatch = (path) =>
  readFileSync(new URL(`../../${path}`, import.meta.url));
const report = Object.fromEntries(
  remediations.map((item) => [
    item.package,
    [
      {
        url: `https://github.com/advisories/${item.advisory}`,
        severity: "high",
      },
    ],
  ])
);

test("only the exact bound remediation accepts a known advisory", () => {
  const verified = verifiedRemediations(manifest, lock, readPatch);
  expect(auditFailures(report, verified)).toEqual([]);
  expect(auditFailures(report)).toHaveLength(2);
  expect(() =>
    verifiedRemediations(manifest, lock, () => "modified patch")
  ).toThrow();
  expect(() =>
    verifiedRemediations(
      { ...manifest, patchedDependencies: {} },
      lock,
      readPatch
    )
  ).toThrow();
  expect(() =>
    verifiedRemediations(
      manifest,
      { ...lock, patchedDependencies: {} },
      readPatch
    )
  ).toThrow();
  const changed = structuredClone(lock);
  changed.packages["nested/braces"] = ["braces@3.0.4", "", {}];
  expect(() => verifiedRemediations(manifest, changed, readPatch)).toThrow();
});

test("new and malformed advisories fail closed, including low severity", () => {
  const verified = verifiedRemediations(manifest, lock, readPatch);
  expect(
    auditFailures(
      {
        braces: [
          {
            url: "https://github.com/advisories/GHSA-new-issue",
            severity: "low",
          },
        ],
      },
      verified
    )
  ).toHaveLength(1);
  expect(auditFailures({ other: report.braces }, verified)).toHaveLength(1);
  expect(
    auditFailures(
      { braces: [{ url: "https://example.com/unexpected" }] },
      verified
    )
  ).toHaveLength(1);
  expect(() => auditFailures([])).toThrow();
  expect(() => auditFailures({ braces: {} })).toThrow();
  expect(() => auditFailures({ braces: [null] })).toThrow();
  expect(auditFailures({}, verified)).toEqual([]);
});
