import type { EventType } from "@norbelys/sdk";

import { humanize } from "@/lib/format";

/**
 * What each event type reports, in the words shown beside its checkbox. It is keyed by the SDK's
 * own union of event types, so a type the API adds fails to compile until it is described here,
 * and no type can be offered that the API would refuse.
 */
const DESCRIPTIONS: Record<EventType, string> = {
  "message.queued": "A message was accepted for sending.",
  "message.sent": "The recipient's server accepted a message.",
  "message.failed": "A message failed for good.",
  "message.uncertain":
    "A submission ended without a readable answer: the message may have been sent.",
  "message.cancelled": "A queued message was cancelled.",
  "message.snippets_fallback":
    "A step's message was created without its personalisation snippets, with the template's defaults.",
  "delivery_event.recorded":
    "Evidence about a message after submission: a delivery, a bounce, a complaint.",
  "inbound_message.received": "A connected inbox received a message.",
  "enrollment.stopped": "An enrollment stopped before its last step.",
  "enrollment.completed": "An enrollment went through its last step.",
  "campaign.status_changed":
    "A campaign started, paused, was archived or ran out of people.",
  "connection.health_changed":
    "A mailbox was paused, disabled, lost its authorization, was archived or recovered.",
  "import.completed": "An import finished.",
  "export.completed": "An export is ready to download.",
  "suppression.created": "An address was suppressed.",
  "ai.budget_warning": "The month's AI spend reached 80% of its budget.",
  "ai.budget_exceeded": "The month's AI spend reached its budget.",
  "endpoint.test": "A test event, sent on request.",
  "webhook_endpoint.disabled":
    "A webhook endpoint was disabled: it answered 410, kept failing, or a person disabled it.",
};

/** Whether a string the API or the address bar gave is an event type this dashboard knows. */
export const isEventType = (value: string): value is EventType =>
  Object.hasOwn(DESCRIPTIONS, value);

/** Every event type an endpoint can subscribe to, in the API's order. */
export const EVENT_TYPES: EventType[] =
  Object.keys(DESCRIPTIONS).filter(isEventType);

/** What an event type reports; a type newer than this dashboard has no description. */
export const describeEventType = (type: string): string | undefined =>
  isEventType(type) ? DESCRIPTIONS[type] : undefined;

/**
 * The events a new endpoint is offered: the outcomes of sending, replies, stops and the health
 * of campaigns and mailboxes. People untick or add the rest before creating it.
 */
export const DEFAULT_EVENT_TYPES: EventType[] = [
  "message.sent",
  "message.failed",
  "delivery_event.recorded",
  "inbound_message.received",
  "enrollment.stopped",
  "campaign.status_changed",
  "connection.health_changed",
  "suppression.created",
];

/** The heading of each prefix, where its words alone would read wrong. */
const PREFIX_LABELS: Record<string, string> = {
  ai: "AI budget",
  connection: "Mailbox connections",
  delivery_event: "Delivery events",
  endpoint: "Tests",
  inbound_message: "Inbound messages",
  webhook_endpoint: "Webhook endpoints",
};

/** The event types that share a prefix (`message.*`), under one heading. */
interface EventTypeGroup {
  prefix: string;
  label: string;
  types: EventType[];
}

const prefixOf = (type: string): string => type.split(".")[0] ?? type;

const labelOf = (prefix: string): string =>
  PREFIX_LABELS[prefix] ?? `${humanize(prefix)}s`;

const groupByPrefix = (types: EventType[]): EventTypeGroup[] => {
  const groups: EventTypeGroup[] = [];
  for (const type of types) {
    const prefix = prefixOf(type);
    const group = groups.find((candidate) => candidate.prefix === prefix);
    if (group) {
      group.types.push(type);
    } else {
      groups.push({ label: labelOf(prefix), prefix, types: [type] });
    }
  }
  return groups;
};

/** The event types grouped by their prefix, in the order each prefix first appears. */
export const EVENT_TYPE_GROUPS: EventTypeGroup[] = groupByPrefix(EVENT_TYPES);

/** The filter options of a list of events: every type, after "All types". */
export const EVENT_TYPE_OPTIONS = [
  { label: "All types", value: "all" },
  ...EVENT_TYPES.map((type) => ({ label: type, value: type })),
];

/**
 * The subscription list to send. Types newer than this page (the API knows them, this build's
 * SDK does not yet) are sent as they came back from the API, so saving an endpoint never drops
 * them; the API refuses anything that is not a type, with a field problem the form shows.
 */
export const toSubscription = (types: readonly string[]): EventType[] =>
  // The cast only widens what the API itself returned or what `EVENT_TYPES` offered.
  types as EventType[];
