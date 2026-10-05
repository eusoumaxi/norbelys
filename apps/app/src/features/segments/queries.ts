import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every segment query starts with this key, so one invalidation refreshes lists and details. */
export const segmentsKey = (workspace: Workspace) =>
  [workspace.id, "segments"] as const;

/** Segments page by page, newest first; a list leaves their counts out. */
export const segmentListQuery = (workspace: Workspace) =>
  listQuery([...segmentsKey(workspace), "list"], (cursor, signal) =>
    workspace.api.segments.list({ cursor, limit: 50 }, { signal })
  );

/** The first 100 segments, newest first: the choices of a picker, and the filters they hold. */
export const segmentOptionsQuery = (workspace: Workspace) =>
  queryOptions({
    queryFn: async ({ signal }) => {
      const { data } = await workspace.api.segments.list(
        { limit: 100 },
        { signal }
      );
      return data;
    },
    queryKey: [...segmentsKey(workspace), "options"],
  });

export const segmentKey = (workspace: Workspace, id: string) =>
  [...segmentsKey(workspace), "detail", id] as const;

/** One segment with its people counted now (up to 10,000), as `segments.retrieve` reads it. */
export const segmentQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryFn: ({ signal }) => workspace.api.segments.retrieve(id, { signal }),
    queryKey: segmentKey(workspace, id),
  });
