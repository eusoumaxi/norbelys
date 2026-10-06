import { APIError } from "@norbelys/sdk";
import type {
  ConnectionObject,
  Provider,
  QuotaScopeObject,
  UpdateConnection,
} from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import { isProvider } from "@/features/mailboxes/providers";
import type { Workspace } from "@/lib/workspace";

/** Every query about the workspace's connections starts with this key. */
export const connectionsKey = (workspace: Workspace) =>
  [workspace.id, "connections"] as const;

/** Every query about the workspace's quota scopes starts with this key. */
export const quotaScopesKey = (workspace: Workspace) =>
  [workspace.id, "quota_scopes"] as const;

/** The connection list, filtered by the start of the account's address when `q` is given. */
export const connectionListQuery = (workspace: Workspace, q: string) =>
  listQuery<ConnectionObject>(
    [...connectionsKey(workspace), "list", q],
    (cursor, signal) =>
      workspace.api.connections.list(
        { cursor, limit: 50, q: q || undefined },
        { signal }
      )
  );

/** How often a connection whose check is running is read again, in milliseconds. */
const CHECKING_POLL_MS = 4000;

export type ConnectionGroup =
  | "mailboxes"
  | "services"
  | "norbelys"
  | "managed-mailboxes";

/** Provider groups retain the API cursor and skip pages containing only another group. */
export const connectionGroupQuery = (
  workspace: Workspace,
  group: ConnectionGroup,
  q: string
) =>
  ({
    ...listQuery<ConnectionObject>(
      [...connectionsKey(workspace), "list", group, q],
      async (cursor, signal) => {
        let next = cursor;
        const visited = new Set<string>();
        for (;;) {
          // oxlint-disable-next-line no-await-in-loop -- each cursor comes from the preceding page
          const page = await workspace.api.connections.list(
            {
              cursor: next,
              limit: 100,
              provider:
                group === "norbelys" || group === "managed-mailboxes"
                  ? "norbelys"
                  : undefined,
              q: q || undefined,
            },
            { signal }
          );
          const data = page.data.filter((connection) => {
            if (
              (group === "norbelys" || group === "managed-mailboxes") &&
              connection.status === "archived"
            ) {
              return false;
            }
            const mailbox = ["google", "microsoft", "smtp"].includes(
              connection.provider
            );
            if (group === "norbelys") {
              return (
                connection.provider === "norbelys" &&
                !connection.account.email.includes("@")
              );
            }
            if (group === "managed-mailboxes") {
              return (
                connection.provider === "norbelys" &&
                connection.account.email.includes("@")
              );
            }
            return group === "mailboxes"
              ? mailbox
              : !mailbox && connection.provider !== "norbelys";
          });
          if (
            data.length > 0 ||
            !page.meta.has_more ||
            !page.meta.next_cursor
          ) {
            return { data, meta: page.meta };
          }
          if (
            page.meta.next_cursor === next ||
            visited.has(page.meta.next_cursor)
          ) {
            throw new Error("The mailbox list returned the same cursor twice.");
          }
          visited.add(page.meta.next_cursor);
          next = page.meta.next_cursor;
        }
      }
    ),
    refetchInterval: (query) =>
      query.state.data?.pages.some((page) =>
        page.data.some((connection) => connection.status === "verifying")
      )
        ? CHECKING_POLL_MS
        : false,
  }) satisfies ReturnType<typeof connectionListQuery>;

/**
 * The workspace's first 100 mailboxes: the choices of a mailbox filter, through their identities
 * those of a message's sender (the API has no lighter list of identities), and the overview's
 * count.
 */
export const mailboxOptionsQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [...connectionsKey(workspace), "options"],
    queryFn: async ({ signal }) =>
      await workspace.api.connections.list({ limit: 100 }, { signal }),
  });

/** One connection; read again every few seconds while its check runs (`verifying`). */
export const connectionQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryFn: ({ signal }) => workspace.api.connections.retrieve(id, { signal }),
    queryKey: [...connectionsKey(workspace), "detail", id],
    refetchInterval: (query) =>
      query.state.data?.status === "verifying" ? CHECKING_POLL_MS : false,
  });

/** The quota scope list, of one provider when `provider` is given. */
export const quotaScopeListQuery = (workspace: Workspace, provider: string) =>
  listQuery<QuotaScopeObject>(
    [...quotaScopesKey(workspace), "list", provider],
    (cursor, signal) =>
      workspace.api.quotaScopes.list(
        {
          cursor,
          limit: 50,
          provider: isProvider(provider) ? provider : undefined,
        },
        { signal }
      )
  );

/** The first hundred quota scopes of one provider, for a choice (a workspace holds a few). */
export const providerScopesQuery = (workspace: Workspace, provider: Provider) =>
  queryOptions({
    queryFn: async ({ signal }) => {
      const page = await workspace.api.quotaScopes.list(
        { limit: 100, provider },
        { signal }
      );
      return page.data;
    },
    queryKey: [...quotaScopesKey(workspace), "provider", provider],
  });

/** The outcome of a guarded replacement: saved, or refused because the list moved meanwhile. */
type Guarded =
  | { saved: ConnectionObject; changed?: never }
  | { changed: ConnectionObject; saved?: never };

/** How many times an update is built when the connection moves between its read and its write. */
const ATTEMPTS = 2;

/**
 * Sends the update `build` makes of a fresh read of the connection, with that read's version in
 * `If-Match`. A list a connection holds (its senders, the folders it reads) is replaced whole,
 * so the replacement is built from what the API holds now, never from the page's older copy: a
 * change made meanwhile to another part of the list is carried over, not undone. `build` answers
 * `null` to refuse, when the part it replaces moved since the editor read it; the fresh
 * connection is then returned as `changed`.
 *
 * The connection's `version` also moves whenever a check runs or the sender paces it, so the
 * version read with the page would refuse most saves of an active mailbox. A write refused by
 * `If-Match` (another change landed between the read and the write) is built again once from a
 * new read; `build` is pure, so building again is safe.
 */
export const updateFromFresh = async (
  workspace: Workspace,
  id: string,
  build: (fresh: ConnectionObject) => UpdateConnection | null,
  attempts = ATTEMPTS
): Promise<Guarded> => {
  const fresh = await workspace.api.connections.retrieve(id);
  const body = build(fresh);
  if (body === null) {
    return { changed: fresh };
  }
  try {
    const saved = await workspace.api.connections.update(id, body, {
      headers: { "If-Match": `"${fresh.version}"` },
    });
    return { saved };
  } catch (error) {
    if (error instanceof APIError && error.status === 412 && attempts > 1) {
      return await updateFromFresh(workspace, id, build, attempts - 1);
    }
    throw error;
  }
};

/**
 * Replaces a list a connection holds (the folders it reads) only when nobody changed that list
 * since the editor read it (`expected`, as `pick` selects it), through `updateFromFresh`.
 */
export const updateGuarded = <T>(
  workspace: Workspace,
  id: string,
  pick: (connection: ConnectionObject) => T,
  expected: T,
  body: UpdateConnection
): Promise<Guarded> =>
  updateFromFresh(workspace, id, (fresh) =>
    JSON.stringify(pick(fresh)) === JSON.stringify(expected) ? body : null
  );
