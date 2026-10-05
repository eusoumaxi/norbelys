import type { IllustrationName } from "@brand/illustrations";
import type { IconSvgElement } from "@hugeicons/react";
import { HugeiconsIcon } from "@hugeicons/react";
import { useInfiniteQuery } from "@tanstack/react-query";
import type {
  InfiniteData,
  QueryClient,
  UseInfiniteQueryOptions,
} from "@tanstack/react-query";
import { cn } from "cn";
import type { ReactNode } from "react";

import { Illustration } from "@/components/illustration";
import { ProblemPanel } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { Workspace } from "@/lib/workspace";

export interface Column<T> {
  id: string;
  header: ReactNode;
  render: (row: T) => ReactNode;
  className?: string;
}

interface EmptyState {
  icon: IconSvgElement;
  /** A drawing from the brand library, shown instead of the icon on a list's first empty state. */
  illustration?: IllustrationName;
  title: string;
  description: ReactNode;
  action?: ReactNode;
}

/** A page of any list the API returns. */
export interface ListPage<T> {
  data: readonly T[];
  meta: { has_more: boolean; next_cursor?: string | null };
}

const SKELETON_ROWS = ["a", "b", "c", "d", "e"];

/** What a list or a section shows while it holds nothing: an icon, a title, a line, an action. */
export const EmptyPanel = ({
  action,
  description,
  icon,
  illustration,
  title,
}: EmptyState) => (
  <div className="border-line rounded-sm border">
    <Empty>
      {illustration ? (
        <Illustration className="mb-1" name={illustration} />
      ) : (
        <EmptyMedia>
          <HugeiconsIcon icon={icon} />
        </EmptyMedia>
      )}
      <EmptyHeader>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription>{description}</EmptyDescription>
      </EmptyHeader>
      {action ? <EmptyContent>{action}</EmptyContent> : null}
    </Empty>
  </div>
);

/** Rows with the console's look; a row can open its detail. */
export const DataTable = <T,>({
  columns,
  empty,
  footer,
  loading = false,
  onRowClick,
  rowKey,
  rows,
  shell = false,
}: {
  columns: Column<T>[];
  empty?: EmptyState;
  footer?: ReactNode;
  loading?: boolean;
  onRowClick?: (row: T) => void;
  rowKey: (row: T) => string;
  rows: readonly T[];
  shell?: boolean;
}) => {
  if (!loading && rows.length === 0 && empty) {
    return <EmptyPanel {...empty} />;
  }
  return (
    <div className="flex min-w-0 flex-col">
      <Table shell={shell}>
        <TableHeader>
          <TableRow>
            {columns.map((column) => (
              <TableHead className={column.className} key={column.id}>
                {column.header}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {loading
            ? SKELETON_ROWS.map((key) => (
                <TableRow key={key}>
                  {columns.map((column) => (
                    <TableCell className={column.className} key={column.id}>
                      <Skeleton className="h-3.5 w-[70%] max-w-40" />
                    </TableCell>
                  ))}
                </TableRow>
              ))
            : rows.map((row) => (
                <TableRow
                  className={onRowClick ? "cursor-pointer" : undefined}
                  key={rowKey(row)}
                  onClick={
                    onRowClick
                      ? (event) => {
                          // A click on a control inside the row (a menu, a link) is its own.
                          if (
                            (event.target as HTMLElement).closest(
                              "a,button,[role=menuitem]"
                            )
                          ) {
                            return;
                          }
                          onRowClick(row);
                        }
                      : undefined
                  }
                >
                  {columns.map((column) => (
                    <TableCell className={column.className} key={column.id}>
                      {column.render(row)}
                    </TableCell>
                  ))}
                </TableRow>
              ))}
        </TableBody>
      </Table>
      {footer}
    </div>
  );
};

/**
 * A list read page by page with the API's cursors: the table, a "Load more" row while pages
 * remain, a skeleton on the first load, and the problem when it fails.
 */
export const ListTable = <T,>({
  query,
  ...props
}: Omit<Parameters<typeof DataTable<T>>[0], "rows" | "loading" | "footer"> & {
  query: ListQuery<T>;
}) => {
  const list = useInfiniteQuery(query);
  if (list.isError) {
    return (
      <ProblemPanel
        error={list.error}
        onRetry={() => {
          void list.refetch();
        }}
      />
    );
  }
  const rows = list.data?.pages.flatMap((page) => page.data) ?? [];
  return (
    <DataTable
      {...props}
      footer={
        list.hasNextPage ? (
          <div className="flex justify-center pt-4">
            <Button
              disabled={list.isFetchingNextPage}
              onClick={() => {
                void list.fetchNextPage();
              }}
              size="s"
              variant="secondary"
            >
              {list.isFetchingNextPage ? <Spinner /> : null}
              Load more
            </Button>
          </div>
        ) : null
      }
      loading={list.isPending}
      rows={rows}
    />
  );
};

/** The infinite-query options of one list: its pages, read with the API's cursors. */
export type ListQuery<T> = UseInfiniteQueryOptions<
  ListPage<T>,
  Error,
  InfiniteData<ListPage<T>>,
  readonly unknown[],
  string | undefined
>;

/** Infinite-query options for a list call of the SDK (or the dashboard client), 50 at a time. */
export const listQuery = <T,>(
  queryKey: readonly unknown[],
  load: (
    cursor: string | undefined,
    signal: AbortSignal
  ) => PromiseLike<ListPage<T>>
): ListQuery<T> => ({
  getNextPageParam: (last) =>
    (last.meta.has_more && last.meta.next_cursor) || undefined,
  initialPageParam: undefined,
  queryFn: async ({ pageParam, signal }) => await load(pageParam, signal),
  queryKey,
});

/**
 * A list of the dashboard's own surface of a workspace (`/v1/workspaces/{id}/…`: members,
 * invitations, API keys, the audit log, single sign-on), which the public SDK does not cover: its
 * pages, 50 at a time, narrowed by `filters`.
 */
export const dashboardListQuery = <T,>(
  workspace: Workspace,
  queryKey: readonly unknown[],
  path: string,
  filters?: Record<string, string>
): ListQuery<T> =>
  listQuery(queryKey, (cursor, signal) => {
    const query = new URLSearchParams({ ...filters, limit: "50" });
    if (cursor) {
      query.set("cursor", cursor);
    }
    return workspace.session.workspace<ListPage<T>>(
      workspace.id,
      "GET",
      `${path}?${query}`,
      { signal }
    );
  });

/**
 * An item one of the loaded lists under `prefix` already holds, with when that list was read: a
 * detail shows it at once (as its placeholder or initial data) instead of a spinner.
 */
export const listedItem = <T extends { id: string }>(
  queryClient: QueryClient,
  prefix: readonly unknown[],
  id: string
): { item: T; updatedAt: number } | undefined => {
  const lists = queryClient.getQueriesData<InfiniteData<ListPage<T>>>({
    queryKey: prefix,
  });
  for (const [queryKey, data] of lists) {
    const item = data?.pages
      .flatMap((page) => page.data)
      .find((candidate) => candidate.id === id);
    if (item) {
      const updatedAt = queryClient.getQueryState(queryKey)?.dataUpdatedAt;
      return { item, updatedAt: updatedAt ?? 0 };
    }
  }
};

/** Text that may be missing: a quiet dash. */
export const Dash = () => <span className="text-fg-4">—</span>;

/** The name cell of a list: 13px bold, an optional 16px icon before it. */
export const NameCell = ({
  children,
  className,
  icon,
}: {
  children: ReactNode;
  className?: string;
  icon?: IconSvgElement;
}) => (
  <span className={cn("flex min-w-0 items-center gap-2", className)}>
    {icon ? (
      <HugeiconsIcon className="text-icon size-4 shrink-0" icon={icon} />
    ) : null}
    <span className="text-fg truncate font-bold">{children}</span>
  </span>
);

/** Someone in a list: the name over the address, or the address alone. */
export const ContactCell = ({
  className,
  email,
  name,
}: {
  className?: string;
  email: string;
  name?: string | null;
}) => (
  <span className={cn("flex min-w-0 flex-col", className)}>
    <span className="text-fg truncate font-bold">{name || email}</span>
    {name ? <span className="text-fg-3 truncate text-xs">{email}</span> : null}
  </span>
);
