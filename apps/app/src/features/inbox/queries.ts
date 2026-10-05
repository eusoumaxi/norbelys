import type { InboundClassification, ThreadStatus } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** What a message the inbox read was classified as, in the order the filters list them. */
export const CLASSIFICATIONS = [
  "human_reply",
  "auto_reply",
  "out_of_office",
  "bounce",
  "complaint",
  "address_change",
  "unsubscribe",
  "unknown",
] as const satisfies readonly InboundClassification[];

export const THREAD_STATUSES = [
  "open",
  "snoozed",
  "archived",
] as const satisfies readonly ThreadStatus[];

/** Every thread query starts with this key, so one invalidation refreshes lists and details. */
export const threadsKey = (workspace: Workspace) =>
  [workspace.id, "threads"] as const;

/** Every inbound message query starts with this key. */
export const inboundKey = (workspace: Workspace) =>
  [workspace.id, "inbound_messages"] as const;

/** The filters of a conversation list, each a `threads.list` filter under the same name. */
interface ThreadFilters {
  classification?: InboundClassification;
  connection_id?: string;
  person_id?: string;
  status?: ThreadStatus;
}

/** Conversations matching `filters`, the latest activity first (the inbox's order), 50 a page. */
export const threadListQuery = (workspace: Workspace, filters: ThreadFilters) =>
  listQuery([...threadsKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.threads.list(
      { ...filters, cursor, limit: 50, sort: "last_activity_at" },
      { signal }
    )
  );

/** One conversation with its latest 50 messages, outbound and inbound, oldest first. */
export const threadQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...threadsKey(workspace), "detail", id],
    queryFn: ({ signal }) => workspace.api.threads.retrieve(id, { signal }),
  });

/**
 * The filters of the received mail list: `inbound_messages.list` filters under the same names,
 * except `review`, whose `pending` asks for `review_requested=true`.
 */
export interface InboundFilters {
  classification?: InboundClassification;
  connection_id?: string;
  review?: "pending";
  thread_id?: string;
}

/** Messages the inbox read matching `filters`, newest first, 50 a page. */
export const inboundListQuery = (
  workspace: Workspace,
  { review, ...filters }: InboundFilters
) =>
  listQuery(
    [...inboundKey(workspace), "list", { ...filters, review }],
    (cursor, signal) =>
      workspace.api.inboundMessages.list(
        {
          ...filters,
          cursor,
          limit: 50,
          review_requested: review === "pending" ? true : undefined,
        },
        { signal }
      )
  );

/** One message the inbox read. */
export const inboundQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...inboundKey(workspace), "detail", id],
    queryFn: ({ signal }) =>
      workspace.api.inboundMessages.retrieve(id, { signal }),
  });
