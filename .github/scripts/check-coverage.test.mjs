import { expect, test } from "bun:test";

import { lineCoverage } from "./check-coverage.mjs";

test("coverage uses covered lines across files rather than averaging percentages", () => {
  expect(lineCoverage("LF:1\nLH:0\nLF:99\nLH:99\n")).toBe(99);
});
test("missing, empty and contradictory reports cannot pass a coverage gate", () => {
  for (const report of ["", "LF:0\nLH:0\n", "LF:2\n", "LF:2\nLH:3\n"]) {
    expect(() => lineCoverage(report)).toThrow();
  }
});
