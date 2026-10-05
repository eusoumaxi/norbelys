import type {
  CampaignObject,
  OnSenderRemoved,
  StopOnReply,
  UpdateCampaign,
} from "@norbelys/sdk";

import { fromLocalInput, toLocalInput } from "@/lib/format";
import { unplacedProblems } from "@/lib/problem";

/** A campaign's settings as the form holds them while they are edited. */
export interface SettingsDraft {
  timezone: string;
  /** A `datetime-local` value in the browser's time zone; empty for none. */
  start_at: string;
  windowed: boolean;
  days: number[];
  start: string;
  end: string;
  identity_ids: string[];
  /** The tags as typed, separated by commas. */
  tags: string;
  on_sender_removed: OnSenderRemoved;
  opens: boolean;
  clicks: boolean;
  /** A sending domain's id, or empty for the platform's tracking host. */
  domain_id: string;
  on_reply: StopOnReply;
  company_on_reply: boolean;
  cooldown_hours: number;
}

/** The form's values for `campaign`, as the API holds them. */
export const settingsOf = (campaign: CampaignObject): SettingsDraft => {
  const { schedule, senders, stop_rules: stop, tracking } = campaign;
  const window = schedule.send_window;
  return {
    clicks: tracking.clicks,
    company_on_reply: stop.company_on_reply,
    cooldown_hours: stop.cooldown_hours,
    days: window ? [...window.days] : [1, 2, 3, 4, 5],
    domain_id: tracking.domain_id ?? "",
    end: window?.end ?? "17:00",
    identity_ids: [...senders.identity_ids],
    on_reply: stop.on_reply,
    on_sender_removed: senders.on_sender_removed,
    opens: tracking.opens,
    start: window?.start ?? "09:00",
    start_at: schedule.start_at ? toLocalInput(schedule.start_at) : "",
    tags: senders.tags.join(", "),
    timezone: schedule.timezone,
    windowed: window !== null && window !== undefined,
  };
};

/** The tags typed: trimmed, without empty ones, each once. */
export const parseTags = (text: string): string[] => [
  ...new Set(
    text
      .split(",")
      .map((tag) => tag.trim())
      .filter(Boolean)
  ),
];

/** Every setting as `campaigns.update` takes it, section by section. */
const sections = (draft: SettingsDraft) => ({
  schedule: {
    send_window: draft.windowed
      ? {
          days: draft.days.toSorted((a, b) => a - b),
          end: draft.end.trim(),
          start: draft.start.trim(),
        }
      : null,
    start_at: fromLocalInput(draft.start_at) ?? null,
    timezone: draft.timezone.trim(),
  },
  senders: {
    identity_ids: draft.identity_ids.toSorted(),
    on_sender_removed: draft.on_sender_removed,
    tags: parseTags(draft.tags).toSorted(),
  },
  stop_rules: {
    company_on_reply: draft.company_on_reply,
    cooldown_hours: draft.cooldown_hours,
    on_reply: draft.on_reply,
  },
  tracking: {
    clicks: draft.clicks,
    domain_id: draft.domain_id || null,
    opens: draft.opens,
  },
});

/** The members of `next` whose JSON differs from `before`'s. */
const changed = (
  next: Record<string, unknown>,
  before: Record<string, unknown>
): Record<string, unknown> =>
  Object.fromEntries(
    Object.entries(next).filter(
      ([key, value]) => JSON.stringify(value) !== JSON.stringify(before[key])
    )
  );

/**
 * The update that turns `base`'s settings into `draft`: only the members that changed, so the
 * API keeps every other value as it is (a member left out of an update is kept).
 */
export const settingsUpdate = (
  draft: SettingsDraft,
  base: CampaignObject
): UpdateCampaign => {
  const next = sections(draft);
  const before = sections(settingsOf(base));
  const update: UpdateCampaign = {};
  for (const key of [
    "schedule",
    "senders",
    "stop_rules",
    "tracking",
  ] as const) {
    const diff = changed(next[key], before[key]);
    if (Object.keys(diff).length > 0) {
      Object.assign(update, { [key]: diff });
    }
  }
  return update;
};

/** Where the API's field errors of each section point (`schedule.send_window`, …). */
const PLACED = [
  "schedule.timezone",
  "schedule.start_at",
  "schedule.send_window",
  "senders.identity_ids",
  "senders.tags",
  "senders.on_sender_removed",
  "tracking.opens",
  "tracking.clicks",
  "tracking.domain_id",
  "stop_rules.on_reply",
  "stop_rules.company_on_reply",
  "stop_rules.cooldown_hours",
];

/** The errors no input of the form shows. */
export const unplacedSettings = (
  errors: Readonly<Record<string, string>>
): [string, string][] => unplacedProblems(errors, PLACED);
