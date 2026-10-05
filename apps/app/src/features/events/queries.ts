import type { EventObject, EventType } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { ListQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every query about the workspace's events starts with this key. */
export const eventsKey = (workspace: Workspace) =>
  [workspace.id, "events"] as const;

/** The workspace's events, newest first; `type` keeps one type. */
export const eventListQuery = (
  workspace: Workspace,
  type: EventType | undefined
): ListQuery<EventObject> =>
  listQuery(
    [...eventsKey(workspace), "list", type ?? "all"],
    (cursor, signal) =>
      workspace.api.events.list({ cursor, limit: 50, type }, { signal })
  );

/** One event, with the data its consumers receive. */
export const eventQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...eventsKey(workspace), "detail", id],
    queryFn: ({ signal }) => workspace.api.events.retrieve(id, { signal }),
  });
