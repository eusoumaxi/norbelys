import { expect, test } from "bun:test";

import {
  collectAudience,
  csvCell,
} from "../src/features/newsletters/recipient-export";
import type { Recipients } from "../src/features/newsletters/recipient-export";

const page = (offset: number, more: boolean): Recipients => ({
  data: Array.from({ length: 100 }, (_, index) => ({
    person_id: `per_${offset + index}`,
    email: `reader${offset + index}@example.com`,
    sent: 1,
    opened: 0,
    clicked: 0,
    opens_tracked: true,
  })),
  meta: { has_more: more, next_cursor: more ? String(offset + 100) : null },
});

test("all 20,000 recipients are included across cursor pages", async () => {
  let requests = 0;
  const csv = await collectAudience((cursor) => {
    const offset = Number(cursor ?? 0);
    requests += 1;
    return Promise.resolve(page(offset, offset < 19_900));
  });
  expect(requests).toBe(200);
  expect(csv.split("\r\n")).toHaveLength(20_001);
  expect(csv).toContain('"reader19999@example.com"');
});

test("oversized or incomplete audiences fail instead of silently truncating", async () => {
  await expect(
    collectAudience((cursor) =>
      Promise.resolve(page(Number(cursor ?? 0), true))
    )
  ).rejects.toThrow("exceeds 20,000");
  await expect(
    collectAudience(() =>
      Promise.resolve({ data: [], meta: { has_more: true } })
    )
  ).rejects.toThrow("incomplete page");
});

test("CSV escaping protects quoted addresses and spreadsheet formulas", () => {
  expect(csvCell('a"b@example.com')).toBe('"a""b@example.com"');
  expect(csvCell("+sum@example.com")).toBe('"\'+sum@example.com"');
});
