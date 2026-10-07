import type { MessageObject } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** A message's states, in the order a message lives them; `messages.list` filters by one. */
export const MESSAGE_STATES = [
  "queued",
  "claimed",
  "in_flight",
  "sent",
  "failed",
  "cancelled",
  "uncertain",
  "suppressed",
] as const;

/**
 * What the message list can be narrowed to, as its address keeps it. Each member is a filter of
 * `messages.list` under the same name, so a link can say `?campaign_id=cmp_…` exactly as a program
 * would ask the API.
 */
export interface MessageFilters {
  campaign_id?: string;
  connection_id?: string;
  person_id?: string;
  state?: (typeof MESSAGE_STATES)[number];
  thread_id?: string;
}

/** Every message query starts with this key, so one invalidation refreshes lists, details and histories. */
export const messagesKey = (workspace: Workspace) =>
  [workspace.id, "messages"] as const;

/** The messages matching `filters`, newest first, 50 a page. */
export const messageListQuery = (
  workspace: Workspace,
  filters: MessageFilters
) => ({
  ...listQuery([...messagesKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.messages.list({ ...filters, cursor, limit: 50 }, { signal })
  ),
  refetchInterval: 30_000,
});

/** Accepted messages can still acquire downstream policy, delivery or bounce reports. */
const awaitingOutcome = (message: MessageObject): boolean =>
  ["queued", "claimed", "in_flight", "uncertain"].includes(message.state) ||
  (message.state === "sent" &&
    !["delivered", "failed"].includes(message.delivery?.status ?? "pending"));

/** One message with its latest attempts, its first delivery events and its holds. */
export const messageQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...messagesKey(workspace), "detail", id],
    queryFn: ({ signal }) => workspace.api.messages.retrieve(id, { signal }),
    refetchInterval: (query) =>
      query.state.data && awaitingOutcome(query.state.data) ? 30_000 : false,
  });

/** The stored body, refreshed when a new attempt may have prepared different content. */
export const messageContentQuery = (
  workspace: Workspace,
  id: string,
  revision: readonly [string, number]
) =>
  queryOptions({
    queryKey: [...messagesKey(workspace), "content", id, ...revision],
    queryFn: ({ signal }) => workspace.api.messages.content(id, { signal }),
    refetchInterval: ["queued", "claimed", "in_flight"].includes(revision[0])
      ? 15_000
      : false,
  });

/** A message's delivery events, oldest first. */
export const messageEventsQuery = (workspace: Workspace, id: string) =>
  listQuery([...messagesKey(workspace), "events", id], (cursor, signal) =>
    workspace.api.deliveryEvents.list(
      { cursor, limit: 50, message_id: id, order: "asc" },
      { signal }
    )
  );
