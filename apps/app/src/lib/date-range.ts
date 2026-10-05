import { createParser } from "nuqs";

import { DAY_MS, utcDay } from "./format";

/** Inclusive UTC dates shared by the URL, the controls and API requests. */
export interface DateRange {
  from: string;
  to: string;
}

/** A real calendar date in the API's format; impossible days never silently roll forward. */
export const calendarDay = createParser({
  parse: (value) => {
    if (!/^\d{4}-\d{2}-\d{2}$/u.test(value)) {
      return null;
    }
    const stamp = Date.parse(`${value}T00:00:00Z`);
    return Number.isFinite(stamp) && utcDay(stamp) === value ? value : null;
  },
  serialize: (value) => value,
});

/** The last `days` UTC dates, including the day of `now`. */
export const recentRange = (days: number, now = Date.now()): DateRange => ({
  from: utcDay(now - (days - 1) * DAY_MS),
  to: utcDay(now),
});

/** Why dates cannot be requested, with an optional inclusive day limit. */
export const rangeProblem = (
  { from, to }: DateRange,
  maxDays?: number
): string | null => {
  if (!calendarDay.parse(from) || !calendarDay.parse(to)) {
    return "Choose a start date and an end date.";
  }
  if (from > to) {
    return "The end date must be on or after the start date.";
  }
  if (maxDays && Date.parse(to) - Date.parse(from) >= maxDays * DAY_MS) {
    return `Choose a range of at most ${maxDays} days.`;
  }
  return null;
};

const date = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeZone: "UTC",
});

/** Calendar dates stay in UTC even when the browser is west of Greenwich. */
export const rangeLabel = ({ from, to }: DateRange): string =>
  `${date.format(new Date(`${from}T00:00:00Z`))} – ${date.format(new Date(`${to}T00:00:00Z`))} (UTC)`;
