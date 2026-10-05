/** Enforce total authored line coverage from LCOV, including files Bun reports below the total. */
import { readFileSync } from "node:fs";

export const lineCoverage = (report) => {
  const found = [...report.matchAll(/^LF:(?<lines>\d+)$/gmu)].map((match) =>
    Number(match.groups.lines)
  );
  const hit = [...report.matchAll(/^LH:(?<lines>\d+)$/gmu)].map((match) =>
    Number(match.groups.lines)
  );
  if (
    found.length === 0 ||
    found.length !== hit.length ||
    found.some((lines, index) => hit[index] > lines)
  ) {
    throw new Error("Missing or invalid LCOV file totals.");
  }
  const total = found.reduce((sum, lines) => sum + lines, 0);
  if (total === 0) {
    throw new Error("No instrumented source lines.");
  }
  return (hit.reduce((sum, lines) => sum + lines, 0) / total) * 100;
};
if (import.meta.main) {
  const [file, minimum = "99"] = process.argv.slice(2);
  const coverage = lineCoverage(readFileSync(file, "utf-8"));
  const threshold = Number(minimum);
  if (
    !Number.isFinite(threshold) ||
    threshold < 0 ||
    threshold > 100 ||
    coverage < threshold
  ) {
    throw new Error(
      `Authored line coverage ${coverage.toFixed(2)}% is below ${minimum}%.`
    );
  }
  console.log(
    `Authored line coverage: ${coverage.toFixed(2)}% (minimum ${minimum}%).`
  );
}
