import type { SendWindow } from "@norbelys/sdk";

const count = new Intl.NumberFormat("en");
const compact = new Intl.NumberFormat("en", {
  maximumFractionDigits: 1,
  notation: "compact",
});
const date = new Intl.DateTimeFormat("en", { dateStyle: "medium" });
const time = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeStyle: "short",
});
const relative = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

/** `in 3 days` → `In 3 days`. */
const capitalize = (text: string): string =>
  text.charAt(0).toUpperCase() + text.slice(1);

export const formatCount = (value: number): string => count.format(value);

/** 1.2K, 34M: for figures that only need their order of magnitude. */
export const formatCompact = (value: number): string => compact.format(value);

/** `1 person`, `1,204 people`: a count with its noun; `many` is the noun with an s unless given. */
export const plural = (value: number, one: string, many = `${one}s`): string =>
  `${formatCount(value)} ${value === 1 ? one : many}`;

/** `12.5%` of `whole`, or `null` when `whole` is zero: a share of nothing is no share. */
export const formatRate = (part: number, whole: number): string | null =>
  whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : null;

export const formatDate = (value: string): string =>
  date.format(new Date(value));

export const formatDateTime = (value: string): string =>
  time.format(new Date(value));

const pad = (n: number) => String(n).padStart(2, "0");

/** `2026-10-02`: the day of a moment, in the browser's time zone. */
const localDay = (d: Date) =>
  `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;

/** `16:37`: the minute of a moment, in the browser's time zone. */
const localMinute = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;

/** `2026-10-02 16:37:10`, as the console's details columns show times. */
export const formatTimestamp = (value: string): string => {
  const d = new Date(value);
  return `${localDay(d)} ${localMinute(d)}:${pad(d.getSeconds())}`;
};

/** A moment as a `datetime-local` control holds it: the browser's time zone, to the minute. */
export const toLocalInput = (value: Date | string): string => {
  const d = new Date(value);
  return `${localDay(d)}T${localMinute(d)}`;
};

/** A `datetime-local` value (the browser's time zone) as the instant the API takes; empty is none. */
export const fromLocalInput = (value: string): string | undefined =>
  value ? new Date(value).toISOString() : undefined;

/** A day, in milliseconds. */
export const DAY_MS = 86_400_000;

/** `2026-10-02`: the UTC day of a moment, as the analytics count days. */
export const utcDay = (value: Date | number): string =>
  new Date(value).toISOString().slice(0, 10);

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 3600],
  ["month", 30 * 24 * 3600],
  ["week", 7 * 24 * 3600],
  ["day", 24 * 3600],
  ["hour", 3600],
  ["minute", 60],
];

/** "2 minutes ago", "in 3 days": how long from now, in the largest whole unit. */
export const formatRelative = (value: string, now = Date.now()): string => {
  const seconds = (Date.parse(value) - now) / 1000;
  for (const [unit, size] of UNITS) {
    if (Math.abs(seconds) >= size) {
      return capitalize(relative.format(Math.round(seconds / size), unit));
    }
  }
  return "Just now";
};

const DAY_NAMES = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/** The days of the week as the API numbers them, 1 (Monday) to 7 (Sunday), with their names. */
export const WEEKDAYS = DAY_NAMES.map((label, index) => ({
  label,
  value: index + 1,
}));

const dayName = (day: number): string => DAY_NAMES[day - 1] ?? String(day);

/** `Mon–Fri`, `Mon, Wed, Fri`, `Every day`: the days in order, three or more in a row as a range. */
const formatDays = (days: readonly number[]): string => {
  const sorted = [...new Set(days)].toSorted((a, b) => a - b);
  if (sorted.length === DAY_NAMES.length) {
    return "Every day";
  }
  const runs: number[][] = [];
  for (const day of sorted) {
    const run = runs.at(-1);
    if (run && run.at(-1) === day - 1) {
      run.push(day);
    } else {
      runs.push([day]);
    }
  }
  return runs
    .map((run) => {
      const first = run[0] ?? 0;
      const last = run.at(-1) ?? first;
      return run.length >= 3
        ? `${dayName(first)}–${dayName(last)}`
        : run.map(dayName).join(", ");
    })
    .join(", ");
};

/** `Mon–Fri, 09:00–17:00`: a send window's days and hours, or that there is none. */
export const formatWindow = (window: SendWindow | null | undefined): string =>
  window
    ? `${formatDays(window.days)}, ${window.start}–${window.end}`
    : "Any day, any hour";

/** The same window inside a sentence: `Sends Mon–Fri, 09:00–17:00`, `Sends any day, any hour`. */
export const windowPhrase = (window: SendWindow | null | undefined): string =>
  window ? formatWindow(window) : "any day, any hour";

/** `grp_0195...0001`: the prefix, then the first and last four characters of the rest. */
export const shortId = (id: string): string => {
  const start = id.indexOf("_") + 1;
  return id.length - start > 11
    ? `${id.slice(0, start + 4)}...${id.slice(-4)}`
    : id;
};

/** `snake_case` or `kebab-case` from the API, as words: `human_reply` → `Human reply`. */
export const humanize = (value: string): string =>
  capitalize(value.replaceAll(/[_-]+/gu, " ").trim());

/** `On` or `Off`, for a switch shown as text. */
export const onOff = (value: boolean): string => (value ? "On" : "Off");

/** A message's subject, or that it has none. */
export const formatSubject = (subject: string | null | undefined): string =>
  subject || "(No subject)";

/** A person's full name, given then family, or `""` when they have neither. */
export const formatName = ({
  family_name: family,
  given_name: given,
}: {
  family_name?: string | null;
  given_name?: string | null;
}): string => [given, family].filter(Boolean).join(" ");

/** `Ada Lovelace <ada@example.com>`, or the address alone when there is no name. */
export const formatAddress = ({
  email,
  name,
}: {
  email: string;
  name?: string | null;
}): string => (name ? `${name} <${email}>` : email);

/** `Madrid time`, `New York time`, `UTC`: a zone as people say it, from its IANA name. */
export const zoneName = (zone: string): string => {
  if (zone === "UTC" || zone === "Etc/UTC" || zone === "Etc/GMT") {
    return "UTC";
  }
  const city = zone.split("/").at(-1) ?? zone;
  return `${city.replaceAll("_", " ")} time`;
};

/** `2026-10-05`: the calendar day of a moment in `zone`. */
const dayIn = (value: Date | number, zone: string): string =>
  new Intl.DateTimeFormat("en-CA", {
    day: "2-digit",
    month: "2-digit",
    timeZone: zone,
    year: "numeric",
  }).format(value);

/** `09:00`: the time of a moment in `zone`, on a 24-hour clock. */
export const timeIn = (value: string, zone: string): string =>
  new Intl.DateTimeFormat("en-GB", {
    hour: "2-digit",
    hourCycle: "h23",
    minute: "2-digit",
    timeZone: zone,
  }).format(new Date(value));

/**
 * `today at 14:20`, `tomorrow at 09:00`, `Monday at 09:00`, `Oct 21 at 09:00`: when a moment
 * falls, as the calendar of `zone` (a campaign's) reads it, so a send window's hours are the hours
 * shown. Lowercase, to sit inside a sentence.
 */
export const formatWhenIn = (
  value: string,
  zone: string,
  now = Date.now()
): string => {
  const days = Math.round(
    (Date.parse(dayIn(new Date(value), zone)) - Date.parse(dayIn(now, zone))) /
      DAY_MS
  );
  const at = timeIn(value, zone);
  if (days === 0) {
    return `today at ${at}`;
  }
  if (days === 1) {
    return `tomorrow at ${at}`;
  }
  const day = new Intl.DateTimeFormat("en", {
    ...(days > 1 && days < 7
      ? { weekday: "long" as const }
      : { day: "numeric" as const, month: "short" as const }),
    timeZone: zone,
  }).format(new Date(value));
  return `${day} at ${at}`;
};
