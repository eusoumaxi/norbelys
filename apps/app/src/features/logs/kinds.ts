import type { DeliveryEventKind } from "@norbelys/sdk";

import { statusLabel } from "@/lib/status";

/** The kinds of delivery event the API reports, in the order a message lives them. */
const KINDS: readonly DeliveryEventKind[] = [
  "accepted",
  "deferred",
  "delivered",
  "bounced",
  "rejected",
  "complaint",
  "unsubscribed",
  "address_changed",
  "reported",
];

/** The filter options of the delivery logs: every kind, after "All kinds". */
export const KIND_OPTIONS = [
  { label: "All kinds", value: "all" },
  ...KINDS.map((kind) => ({ label: statusLabel("event", kind), value: kind })),
];

/** Whether a kind from the address bar is one the API filters on. */
export const isKind = (value: string): value is DeliveryEventKind =>
  (KINDS as readonly string[]).includes(value);

/** How far each confidence lets a report be trusted, for the confidence column's hover. */
export const CONFIDENCE: Record<string, string> = {
  authenticated:
    "From the server Norbelys talked to, or signed or verified by its reporter.",
  corroborated:
    "Matches Norbelys's records, but its reporter is not authenticated.",
  human_text: "A notice a person wrote: routed to review, never automatic.",
  inferred: "A partial match: kept for review and counters only.",
};
