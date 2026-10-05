import { listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every suppression query starts with this key, so one invalidation refreshes them all. */
export const suppressionsKey = (workspace: Workspace) =>
  [workspace.id, "suppressions"] as const;

/** The suppressions page by page, newest first. */
export const suppressionListQuery = (workspace: Workspace) =>
  listQuery([...suppressionsKey(workspace), "list"], (cursor, signal) =>
    workspace.api.suppressions.list({ cursor, limit: 50 }, { signal })
  );
