import type { ExportObject, ImportObject } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { ListPage } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every import query starts with this key. */
export const importsKey = (workspace: Workspace) =>
  [workspace.id, "imports"] as const;

/** Every export query starts with this key. */
export const exportsKey = (workspace: Workspace) =>
  [workspace.id, "exports"] as const;

/** An import still at work: its job is waiting or running. */
export const importRunning = (item: Pick<ImportObject, "status">): boolean =>
  item.status === "queued" || item.status === "processing";

/** An export still at work. */
export const exportRunning = (item: Pick<ExportObject, "status">): boolean =>
  item.status === "queued" || item.status === "running";

/** How often a list or an import is read again while something in it is at work. */
const POLL_MS = 2000;

/** Polls a list while one of its loaded rows is still at work, and stops once none is. */
const pollWhile =
  <T>(running: (item: T) => boolean) =>
  (query: { state: { data?: { pages: ListPage<T>[] } } }) =>
    query.state.data?.pages.some((page) => page.data.some(running))
      ? POLL_MS
      : false;

/** Imports page by page, newest first, read again every 2 s while one runs. */
export const importListQuery = (workspace: Workspace) => ({
  ...listQuery([...importsKey(workspace), "list"], (cursor, signal) =>
    workspace.api.imports.list({ cursor, limit: 50 }, { signal })
  ),
  refetchInterval: pollWhile<ImportObject>(importRunning),
});

/** One import with its counts and first problems, read again every 2 s while it runs. */
export const importQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryFn: ({ signal }) => workspace.api.imports.retrieve(id, { signal }),
    queryKey: [...importsKey(workspace), "detail", id],
    refetchInterval: (query) =>
      query.state.data && importRunning(query.state.data) ? POLL_MS : false,
  });

/** Exports page by page, newest first, read again every 2 s while one runs. */
export const exportListQuery = (workspace: Workspace) => ({
  ...listQuery([...exportsKey(workspace), "list"], (cursor, signal) =>
    workspace.api.exports.list({ cursor, limit: 50 }, { signal })
  ),
  refetchInterval: pollWhile<ExportObject>(exportRunning),
});
