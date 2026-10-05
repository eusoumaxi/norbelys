import type { GroupObject } from "@norbelys/sdk";
import type { QueryClient } from "@tanstack/react-query";
import { queryOptions } from "@tanstack/react-query";

import { listedItem, listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every group query starts with this key, so one invalidation refreshes lists and details. */
export const groupsKey = (workspace: Workspace) =>
  [workspace.id, "groups"] as const;

export const groupListQuery = (workspace: Workspace, q: string) =>
  listQuery([...groupsKey(workspace), "list", { q }], (cursor, signal) =>
    workspace.api.groups.list(
      { cursor, limit: 50, q: q || undefined },
      { signal }
    )
  );

export const groupKey = (workspace: Workspace, id: string) =>
  [...groupsKey(workspace), "detail", id] as const;

/** One group; a group a list already loaded opens with no spinner while that list is fresh. */
export const groupQuery = (
  queryClient: QueryClient,
  workspace: Workspace,
  id: string
) => {
  const listed = listedItem<GroupObject>(
    queryClient,
    [...groupsKey(workspace), "list"],
    id
  );
  return queryOptions({
    queryKey: groupKey(workspace, id),
    queryFn: ({ signal }) => workspace.api.groups.retrieve(id, { signal }),
    initialData: listed?.item,
    initialDataUpdatedAt: listed?.updatedAt,
  });
};
