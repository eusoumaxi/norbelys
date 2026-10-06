import type { DomainObject } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every query about the workspace's sending domains starts with this key. */
export const domainsKey = (workspace: Workspace) =>
  [workspace.id, "sending_domains"] as const;

/** The sending domain list, newest first. */
export const domainListQuery = (workspace: Workspace) =>
  listQuery<DomainObject>(
    [...domainsKey(workspace), "list"],
    (cursor, signal) =>
      workspace.api.sendingDomains.list({ cursor, limit: 50 }, { signal })
  );

/** All domain choices, following cursors so older verified domains remain selectable. */
export const domainOptionsQuery = (workspace: Workspace) =>
  queryOptions({
    queryFn: async ({ signal }) => {
      const first = await workspace.api.sendingDomains.list(
        { limit: 100 },
        { signal }
      );
      const data = [...first.data];
      let page = first;
      const visited = new Set<string>();
      while (page.meta.has_more && page.meta.next_cursor) {
        const cursor = page.meta.next_cursor;
        if (visited.has(cursor)) {
          throw new Error("The domain list returned the same cursor twice.");
        }
        visited.add(cursor);
        // oxlint-disable-next-line no-await-in-loop -- each cursor comes from the preceding page
        page = await workspace.api.sendingDomains.list(
          { cursor, limit: 100 },
          { signal }
        );
        data.push(...page.data);
      }
      return { ...page, data };
    },
    queryKey: [...domainsKey(workspace), "options"],
  });

/** Whether a domain is proven: its ownership record resolves (`active` also serves tracking). */
export const domainVerified = (domain: Pick<DomainObject, "status">): boolean =>
  domain.status === "active" || domain.status === "verified";

/** How often a domain whose DNS check runs is read again, in milliseconds. */
const CHECKING_POLL_MS = 3000;

/** One sending domain; read again every few seconds while its DNS check runs (`verifying`). */
export const domainQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryFn: ({ signal }) =>
      workspace.api.sendingDomains.retrieve(id, { signal }),
    queryKey: [...domainsKey(workspace), "detail", id],
    refetchInterval: (query) =>
      query.state.data?.status === "verifying" ||
      query.state.data?.tracking_domain?.status === "verifying" ||
      ((query.state.data?.status === "pending_certificate" ||
        query.state.data?.tracking_domain?.status === "pending_certificate") &&
        query.state.dataUpdateCount < 100) ||
      (query.state.data?.dns_preparation === "preparing" &&
        query.state.dataUpdateCount < 40)
        ? CHECKING_POLL_MS
        : false,
  });

/** What the last check found wrong, from the API's untyped `last_error` (`{code, detail, at}`). */
export const lastError = (
  domain: DomainObject
): { code?: string; detail: string; at?: string } | null => {
  const error: unknown = domain.last_error;
  if (typeof error !== "object" || error === null) {
    return null;
  }
  const text = (key: string) => {
    const value: unknown = Reflect.get(error, key);
    return typeof value === "string" ? value : undefined;
  };
  const detail = text("detail") ?? text("code");
  return detail ? { at: text("at"), code: text("code"), detail } : null;
};
