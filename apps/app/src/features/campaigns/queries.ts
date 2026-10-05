import type {
  AnalyticsObject,
  CampaignObject,
  CampaignStatus,
  ConnectionObject,
  DomainObject,
  EnrollmentStatus,
  EnrollmentSummary,
  GroupBy,
  GroupObject,
  IdentityObject,
  SegmentObject,
} from "@norbelys/sdk";
import { queryOptions, useSuspenseQuery } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";

import { listQuery } from "@/components/data-table";
import { DAY_MS, utcDay } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** Every campaign query starts with this key, so one invalidation refreshes lists and details. */
export const campaignsKey = (workspace: Workspace) =>
  [workspace.id, "campaigns"] as const;

/** One campaign's queries (the object, its reports, whether it sent) share this prefix. */
export const campaignKey = (workspace: Workspace, id: string) =>
  [...campaignsKey(workspace), "detail", id] as const;

/** Enrollment lists of any campaign. */
export const enrollmentsKey = (workspace: Workspace) =>
  [workspace.id, "enrollments"] as const;

export const campaignListQuery = (
  workspace: Workspace,
  filters: { status?: CampaignStatus; q: string }
) =>
  listQuery([...campaignsKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.campaigns.list(
      {
        cursor,
        limit: 50,
        q: filters.q || undefined,
        status: filters.status,
      },
      { signal }
    )
  );

/**
 * The workspace's 100 newest campaigns: the choices of a campaign filter, the names of the
 * campaign ids a list shows, and the overview's count.
 */
export const campaignOptionsQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [...campaignsKey(workspace), "options"],
    queryFn: async ({ signal }) =>
      await workspace.api.campaigns.list({ limit: 100 }, { signal }),
  });

/**
 * A campaign with its steps and their bodies. While it is `materialising` (its start job runs)
 * it is read again every two seconds, so the page turns `active` by itself.
 */
export const campaignQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: campaignKey(workspace, id),
    queryFn: ({ signal }) => workspace.api.campaigns.retrieve(id, { signal }),
    refetchInterval: (query) =>
      query.state.data?.status === "materialising" ? 2000 : false,
  });

/** The campaign of the detail page the person is on, from its loader's cache. */
export const useCampaign = (): CampaignObject => {
  const workspace = useWorkspace();
  const id = useParams({
    from: "/w/$slug/campaigns/$campaignId",
    select: (params) => params.campaignId,
  });
  return useSuspenseQuery(campaignQuery(workspace, id)).data;
};

/** Whether the person may change the campaign: a viewer only reads, and an archived one is done. */
export const campaignEditable = (
  workspace: Workspace,
  campaign: CampaignObject
): boolean => canWrite(workspace) && campaign.status !== "archived";

/**
 * The days a campaign's whole history covers, as far as the analytics answer one read: from
 * its creation (at most 365 days back) to today.
 */
export const lifetime = (campaign: CampaignObject) => {
  const today = Date.now();
  const created = Date.parse(campaign.created_at);
  return {
    from: utcDay(Math.max(created, today - 365 * DAY_MS)),
    to: utcDay(today),
  };
};

/**
 * A campaign's counters from the rollup: per day over the last 30 days, or per step or variant
 * over its whole history.
 */
export const campaignAnalyticsQuery = (
  workspace: Workspace,
  campaign: CampaignObject,
  groupBy: GroupBy
) => {
  const range = groupBy === "day" ? {} : lifetime(campaign);
  return queryOptions<AnalyticsObject>({
    queryKey: [...campaignKey(workspace, campaign.id), "analytics", groupBy],
    queryFn: ({ signal }) =>
      workspace.api.analytics.retrieve(
        { campaign_id: campaign.id, group_by: groupBy, ...range },
        { signal }
      ),
  });
};

/**
 * Whether any message of the campaign exists: deleting such a campaign archives it instead of
 * removing it, because its messages keep pointing at it.
 */
export const hasMessagesQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...campaignKey(workspace, id), "has-messages"],
    queryFn: async ({ signal }) => {
      const page = await workspace.api.messages.list(
        { campaign_id: id, limit: 1 },
        { signal }
      );
      return page.data.length > 0;
    },
  });

/** What a list of enrollments is narrowed to: a campaign's, a person's, a status. */
interface EnrollmentFilters {
  campaign_id?: string;
  person_id?: string;
  status?: EnrollmentStatus;
}

/** Enrollments matching `filters`, newest first, 50 a page. */
export const enrollmentListQuery = (
  workspace: Workspace,
  filters: EnrollmentFilters
) =>
  listQuery([...enrollmentsKey(workspace), "list", filters], (cursor, signal) =>
    workspace.api.enrollments.list(
      { ...filters, cursor, limit: 50 },
      { signal }
    )
  );

/** A page of a list with the SDK's `nextPage()`. */
interface Paged<T> {
  data: readonly T[];
  nextPage: () => Promise<Paged<T> | null>;
}

/** The rows of `page` and of the pages after it, at most `pages` pages in all. */
const allRows = async <T>(page: Paged<T>, pages = 10): Promise<T[]> => {
  const next = pages > 1 ? await page.nextPage() : null;
  const rest = next ? await allRows(next, pages - 1) : [];
  return [...page.data, ...rest];
};

/** A sender identity with the connection (mailbox) it sends through. */
export interface SenderIdentity {
  identity: IdentityObject;
  connection: ConnectionObject;
}

/**
 * Every sender identity of the workspace, from every connection's `identities`: what a pool can
 * name, and how to show an identity id (`sid_…`) an enrollment or a campaign mentions.
 */
export const sendersQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [workspace.id, "connections", "identities"],
    queryFn: async ({ signal }) => {
      const first = await workspace.api.connections.list(
        { limit: 100 },
        { signal }
      );
      const connections = await allRows(first);
      return connections.flatMap((connection) =>
        connection.identities.map((identity): SenderIdentity => ({
          connection,
          identity,
        }))
      );
    },
  });

/** The sending domains whose tracking host a campaign's links can use. */
export const trackingDomainsQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [workspace.id, "sending_domains", "tracking"],
    queryFn: async ({ signal }): Promise<DomainObject[]> => {
      const first = await workspace.api.sendingDomains.list(
        { limit: 100 },
        { signal }
      );
      return await allRows(first);
    },
  });

/** The workspace's groups (up to 1,000), as audiences to enroll. */
export const allGroupsQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [workspace.id, "groups", "all"],
    queryFn: async ({ signal }): Promise<GroupObject[]> => {
      const first = await workspace.api.groups.list({ limit: 100 }, { signal });
      return await allRows(first);
    },
  });

/** The workspace's segments (up to 1,000), as audiences to enroll. */
export const allSegmentsQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [workspace.id, "segments", "all"],
    queryFn: async ({ signal }): Promise<SegmentObject[]> => {
      const first = await workspace.api.segments.list(
        { limit: 100 },
        { signal }
      );
      return await allRows(first);
    },
  });

/**
 * Where a campaign's people are now and when its next email may go (`campaign.enrollments`), or
 * `null` when the answer carries none: a list, or an API that does not count them yet.
 */
export const enrollmentSummary = (
  campaign: CampaignObject
): EnrollmentSummary | null => campaign.enrollments ?? null;
