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
) =>
  listQuery([...messagesKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.messages.list({ ...filters, cursor, limit: 50 }, { signal })
  );

/** One message with its latest attempts, its first delivery events and its holds. */
export const messageQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...messagesKey(workspace), "detail", id],
    queryFn: ({ signal }) => workspace.api.messages.retrieve(id, { signal }),
  });

/** A message's delivery events, oldest first. */
export const messageEventsQuery = (workspace: Workspace, id: string) =>
  listQuery([...messagesKey(workspace), "events", id], (cursor, signal) =>
    workspace.api.deliveryEvents.list(
      { cursor, limit: 50, message_id: id, order: "asc" },
      { signal }
    )
  );
