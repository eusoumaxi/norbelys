import type { Tone } from "@/components/ui/badge";
import { humanize } from "@/lib/format";

/** How a state is drawn: its dot's tone, with its own words when the value's are not right. */
type Look = Tone | { label: string; tone: Tone };

/**
 * How each state the API reports is drawn, by the kind of thing it is the state of. States missing
 * here (the API's enums are open) show as their own words beside a grey dot.
 */
const table = {
  /** How a message's attempt ended. */
  attempt: {
    accepted: "success",
    permanent: { label: "Permanent failure", tone: "error" },
    released: "muted",
    skipped: "muted",
    suppressed: "muted",
    transient: { label: "Temporary failure", tone: "warning" },
    uncertain: "warning",
  },
  campaign: {
    active: "success",
    archived: "muted",
    completed: "neutral",
    draft: "neutral",
    materialising: { label: "Starting", tone: "info" },
    paused: "warning",
  },
  /** What an inbound message was classified as: a person's reply stands out, reports read as problems. */
  classification: {
    address_change: "info",
    auto_reply: { label: "Auto-reply", tone: "neutral" },
    bounce: "error",
    complaint: "error",
    human_reply: { label: "Reply", tone: "accent" },
    out_of_office: "neutral",
    unknown: "muted",
    unsubscribe: "warning",
  },
  /** A mailbox, in the words of the person who owns it: working, or what it needs. */
  connection: {
    active: { label: "Working", tone: "success" },
    archived: { label: "Disconnected", tone: "muted" },
    authorization_required: { label: "Needs reconnecting", tone: "warning" },
    disabled: { label: "Blocked", tone: "error" },
    failed: "error",
    unverified: { label: "Not checked yet", tone: "warning" },
    verifying: { label: "Checking", tone: "info" },
  },
  /** A webhook delivery: an attempt is due, it arrived, its retries are used up, its endpoint is off. */
  delivery: {
    delivered: "success",
    disabled: "muted",
    failed: "error",
    pending: "info",
  },
  domain: {
    active: "success",
    deleting: "muted",
    pending_certificate: "info",
    pending_verification: "warning",
    suspended: "error",
    verified: "success",
    verifying: "info",
  },
  endpoint: { disabled: "muted", enabled: "success", failing: "error" },
  enrollment: {
    active: "success",
    completed: "neutral",
    failed: "error",
    paused: "warning",
    replied: "accent",
    stopped: "muted",
  },
  /** A delivery event: a final refusal is an error, a temporary one a warning, a delivery a success. */
  event: {
    accepted: "info",
    address_changed: "info",
    bounced: "error",
    complaint: "error",
    deferred: "warning",
    delivered: "success",
    rejected: "error",
    reported: "warning",
    unsubscribed: "muted",
  },
  /** An import or an export, from queued to its file. */
  job: {
    completed: "success",
    expired: "muted",
    failed: "error",
    processing: "info",
    queued: "neutral",
    ready: "success",
    running: "info",
  },
  key: { active: "success", expired: "muted", revoked: "muted" },
  message: {
    cancelled: "muted",
    claimed: "info",
    failed: "error",
    in_flight: "info",
    queued: "neutral",
    sent: "success",
    suppressed: "warning",
    uncertain: "warning",
  },
  /** A workspace's mode (and its keys'): live sends for real, test stops at a test transport. */
  mode: { live: "success", test: { label: "Test mode", tone: "warning" } },
  /** What a domain's last DNS check observed of one of its records. */
  record: {
    missing: "error",
    unchecked: { label: "Not checked yet", tone: "muted" },
    verified: { label: "Found", tone: "success" },
  },
  sentiment: { negative: "error", neutral: "neutral", positive: "success" },
  /** Why an address is suppressed: a bounce or a complaint reads as a problem. */
  suppression: { bounce: "error", complaint: "error" },
  thread: { archived: "muted", open: "success", snoozed: "warning" },
  /** An address check's verdict on its routing. */
  verdict: { invalid: "error", routable: "success", unknown: "warning" },
} satisfies Record<string, Record<string, Look>>;

export type StatusKind = keyof typeof table;

const looks: Record<StatusKind, Record<string, Look>> = table;

/** The tone of a state's dot; a state this page does not know is neutral. */
export const statusTone = (kind: StatusKind, value: string): Tone => {
  const look = looks[kind][value];
  if (look === undefined) {
    return "neutral";
  }
  return typeof look === "string" ? look : look.tone;
};

/** A state in words: its own label when it has one, else the value's (`in_flight` → `In flight`). */
export const statusLabel = (kind: StatusKind, value: string): string => {
  const look = looks[kind][value];
  return typeof look === "object" ? look.label : humanize(value);
};
