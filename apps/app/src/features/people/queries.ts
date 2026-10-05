import { keepPreviousData, queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import { groupsKey } from "@/features/groups/queries";
import type { Workspace } from "@/lib/workspace";

/** Every people query starts with this key, so one invalidation refreshes lists and details. */
export const peopleKey = (workspace: Workspace) =>
  [workspace.id, "people"] as const;

/** What a list of people is narrowed to; each combination is its own cached list. */
interface PeopleFilters {
  q?: string;
  groupId?: string | null;
  segmentId?: string | null;
}

/** People page by page, newest first, narrowed by a search, a group or a segment. */
export const peopleListQuery = (workspace: Workspace, filters: PeopleFilters) =>
  listQuery([...peopleKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.people.list(
      {
        cursor,
        group_id: filters.groupId || undefined,
        limit: 50,
        q: filters.q || undefined,
        segment_id: filters.segmentId || undefined,
      },
      { signal }
    )
  );

/**
 * The first people matching `q` (a prefix of the address, a name or the company), newest first;
 * with no `q`, the newest. The choices of a person picker.
 */
export const peopleSearchQuery = (workspace: Workspace, q: string) =>
  queryOptions({
    placeholderData: keepPreviousData,
    queryFn: async ({ signal }) => {
      const { data } = await workspace.api.people.list(
        { limit: 8, q: q || undefined },
        { signal }
      );
      return data;
    },
    queryKey: [...peopleKey(workspace), "search", q],
  });

export const personKey = (workspace: Workspace, id: string) =>
  [...peopleKey(workspace), "detail", id] as const;

/** One person, as `people.retrieve` reads it. */
export const personQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryFn: ({ signal }) => workspace.api.people.retrieve(id, { signal }),
    queryKey: personKey(workspace, id),
  });

/** Every custom field query starts with this key. */
export const fieldsKey = (workspace: Workspace) =>
  [workspace.id, "fields"] as const;

/**
 * The workspace's custom field definitions, oldest first. A workspace holds at most 100, so one
 * page is all of them.
 */
export const fieldsQuery = (workspace: Workspace) =>
  queryOptions({
    queryFn: async ({ signal }) => {
      const { data } = await workspace.api.fields.list(
        { limit: 100, order: "asc" },
        { signal }
      );
      return data;
    },
    queryKey: [...fieldsKey(workspace), "all"],
  });

/**
 * The first 100 groups, newest first: the choices of a group picker and the names of the ids a
 * person carries. Under the groups key, so a change to a group refreshes it.
 */
export const groupOptionsQuery = (workspace: Workspace) =>
  queryOptions({
    queryFn: async ({ signal }) => {
      const { data } = await workspace.api.groups.list(
        { limit: 100 },
        { signal }
      );
      return data;
    },
    queryKey: [...groupsKey(workspace), "options"],
  });
