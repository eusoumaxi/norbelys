import type { CampaignObject, EnrollmentSummary } from "@norbelys/sdk";

import type { Tone } from "@/components/ui/badge";
import { BROWSER_ZONE } from "@/lib/form";
import {
  formatWhenIn,
  humanize,
  plural,
  timeIn,
  windowPhrase,
  zoneName,
} from "@/lib/format";

/** What a campaign is doing now, as the line under its name says it. */
interface CampaignNow {
  /** The state in a word or two, beside its dot. */
  label: string;
  tone: Tone;
  /** Whether it is sending right now: its dot pings. */
  live: boolean;
  /** What happens next, in a sentence. */
  sentence: string;
  /** The next moment on the person's own clock, when their zone is not the campaign's. */
  local?: string;
}

/** Why a campaign cannot send, as a title (the API's detail follows it). */
const BLOCKED: Record<string, string> = {
  invalid: "It could not start",
  no_sender: "No mailbox of its pool can send",
};

/** The people still in the sequence: active or paused enrollments. */
const inSequence = (summary: EnrollmentSummary) =>
  summary.active + summary.paused;

/** Every enrollment the campaign ever had. */
export const enrolled = (summary: EnrollmentSummary) =>
  summary.active +
  summary.paused +
  summary.replied +
  summary.completed +
  summary.stopped +
  summary.failed;

/** What an active campaign does next: its next email, or why there is none. */
const activeNow = (
  campaign: CampaignObject,
  summary: EnrollmentSummary | null,
  now: number
): CampaignNow => {
  const zone = campaign.schedule.timezone;
  if (!summary) {
    return {
      label: "Active",
      live: false,
      sentence: `Sends ${windowPhrase(campaign.schedule.send_window)}, ${zoneName(zone)}.`,
      tone: "success",
    };
  }
  const people = inSequence(summary);
  if (people === 0) {
    return {
      label: "Active",
      live: false,
      sentence:
        enrolled(summary) === 0
          ? "No one is enrolled yet. Enroll people and their first email goes out in the next send window."
          : "Everyone has finished the sequence. Enroll more people to keep it going.",
      tone: "success",
    };
  }
  const next = summary.next_run_at;
  const who = plural(people, "person", "people");
  if (!next) {
    return {
      label: "Active",
      live: false,
      sentence: `${who} in the sequence.`,
      tone: "success",
    };
  }
  // Due now, or emails already on their way: the one moment the dot pings.
  if (Date.parse(next) <= now) {
    return {
      label: "Sending",
      live: true,
      sentence: `Emails are going out now. ${who} in the sequence.`,
      tone: "success",
    };
  }
  const local =
    BROWSER_ZONE !== zone && timeIn(next, BROWSER_ZONE) !== timeIn(next, zone)
      ? `${timeIn(next, BROWSER_ZONE)} your time`
      : undefined;
  return {
    label: "Active",
    live: false,
    local,
    sentence: `Next email ${formatWhenIn(next, zone, now)}, ${zoneName(zone)}. ${who} in the sequence.`,
    tone: "success",
  };
};

/**
 * The line under a campaign's name: its state and what happens next, in Norbelys's words. A
 * problem that stops it from sending comes first; then the state, with the next email's time in
 * the campaign's own zone (where its send window is), and the person's clock beside it when the
 * two differ. `summary` is null while the API does not count enrollments for this answer.
 */
export const campaignNow = (
  campaign: CampaignObject,
  summary: EnrollmentSummary | null,
  now = Date.now()
): CampaignNow => {
  const error = campaign.last_error;
  if (error && campaign.status !== "archived") {
    return {
      label: "Can't send",
      live: false,
      sentence: `${BLOCKED[error.code] ?? humanize(error.code)}: ${error.detail}`,
      tone: "error",
    };
  }
  switch (campaign.status) {
    case "draft": {
      let sentence = "Nothing is sent until you start it.";
      if (campaign.steps.length === 0) {
        sentence =
          "Write its first email, then start it. Nothing is sent before.";
      } else if (summary && enrolled(summary) === 0) {
        sentence =
          "Enroll the people to write to, then start it. Nothing is sent before.";
      }
      return { label: "Draft", live: false, sentence, tone: "neutral" };
    }
    case "materialising": {
      return {
        label: "Starting",
        live: false,
        sentence: "Getting the first emails ready. This takes a few seconds.",
        tone: "info",
      };
    }
    case "active": {
      return activeNow(campaign, summary, now);
    }
    case "paused": {
      return {
        label: "Paused",
        live: false,
        sentence:
          "Nothing is sent until you resume it. Everyone keeps their place in the sequence.",
        tone: "warning",
      };
    }
    case "completed": {
      return {
        label: "Completed",
        live: false,
        sentence: "Everyone has been through the sequence.",
        tone: "neutral",
      };
    }
    default: {
      return {
        label: "Archived",
        live: false,
        sentence: "Nothing is sent. Its history and figures stay here.",
        tone: "muted",
      };
    }
  }
};
