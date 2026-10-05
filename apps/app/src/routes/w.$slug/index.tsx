import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type {
  AnalyticsObject,
  CampaignObject,
  ConnectionObject,
  ThreadObject,
} from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import type { UseQueryResult } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";

import { MetricGroup } from "@/components/details";
import { Illustration } from "@/components/illustration";
import { PageBody, PageHeader } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { RelativeTime } from "@/components/time";
import { Button } from "@/components/ui/button";
import { planLine } from "@/features/campaigns/format";
import { campaignOptionsQuery } from "@/features/campaigns/queries";
import { threadsKey } from "@/features/inbox/queries";
import { mailboxOptionsQuery } from "@/features/mailboxes/queries";
import { ActivityCard } from "@/features/overview/activity-chart";
import { GettingStarted, ListCard } from "@/features/overview/overview-cards";
import type { ListRow, SetupStep } from "@/features/overview/overview-cards";
import { formatCount, formatRate } from "@/lib/format";
import { useWorkspace, workspaceQuery } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** How many items each short list shows. */
const FIRST = 5;

/** Campaigns that are doing something come first, then the ones waiting, then the rest. */
const CAMPAIGN_ORDER = [
  "active",
  "materialising",
  "paused",
  "draft",
  "completed",
  "archived",
];

const useAnalytics = (workspace: Workspace) =>
  useQuery({
    queryFn: ({ signal }) =>
      workspace.api.analytics.retrieve({ group_by: "day" }, { signal }),
    queryKey: [workspace.id, "analytics", "daily"],
  });

/** The latest conversations, for the overview's short list. */
const useLatestConversations = (workspace: Workspace) =>
  useQuery({
    queryFn: async ({ signal }) =>
      await workspace.api.threads.list(
        { limit: FIRST, sort: "last_activity_at", status: "open" },
        { signal }
      ),
    queryKey: [...threadsKey(workspace), "latest"],
  });

/**
 * The last 30 days in four numbers. Before the first report is counted they are "Pending", not
 * zero; rates are of what was sent.
 */
const keyMetrics = (report: AnalyticsObject | undefined) => {
  const counted = Boolean(report?.computed_at);
  const totals = report?.totals;
  const figure = (value: number | undefined) =>
    counted && value !== undefined ? formatCount(value) : "Pending";
  const rate = (value: number | undefined) =>
    counted && totals && value !== undefined
      ? (formatRate(value, totals.sent) ?? undefined)
      : undefined;
  return [
    { label: "Sent", value: figure(totals?.sent) },
    {
      label: "Delivered",
      note: rate(totals?.delivered),
      value: figure(totals?.delivered),
    },
    {
      accent: (totals?.replied ?? 0) > 0,
      label: "Replies",
      note: rate(totals?.replied),
      value: figure(totals?.replied),
    },
    {
      label: "Bounced",
      note: rate(totals?.bounced),
      value: figure(totals?.bounced),
    },
  ];
};

/** The three steps a new workspace takes before it can send, each with its own page. */
const setupSteps = (
  slug: string,
  done: { launched: boolean; mailbox: boolean; people: boolean }
): SetupStep[] => {
  const link = { params: { slug } };
  return [
    {
      action: {
        label: "Connect mailbox",
        link: { ...link, to: "/w/$slug/mailboxes/new" },
      },
      description:
        "Google, Microsoft or any other: your emails go out from your own address.",
      done: done.mailbox,
      title: "Connect a mailbox",
    },
    {
      action: {
        label: "Import people",
        link: { ...link, to: "/w/$slug/imports" },
      },
      description:
        "Upload a spreadsheet (CSV). Addresses are checked before anything is sent.",
      done: done.people,
      title: "Add the people to write to",
    },
    {
      action: {
        label: "New campaign",
        link: { ...link, search: { new: true }, to: "/w/$slug/campaigns" },
      },
      description: "Write the emails, choose who gets them and when.",
      done: done.launched,
      title: "Start your first campaign",
    },
  ];
};

/** A mailbox in the short list: its address, how much it sent today, its health. */
const mailboxRow = (m: ConnectionObject, slug: string): ListRow => ({
  key: m.id,
  link: {
    params: { connectionId: m.id, slug },
    to: "/w/$slug/mailboxes/$connectionId",
  },
  primary: m.account.email,
  secondary: m.paused
    ? "Paused"
    : `${formatCount(m.usage.today.used)} of ${formatCount(m.daily_limit)} sent today`,
  trailing: <StatusBadge kind="connection" value={m.status} />,
});

/** A campaign in the short list: its name, its plan, its state. */
const campaignRow = (c: CampaignObject, slug: string): ListRow => ({
  key: c.id,
  link: {
    params: { campaignId: c.id, slug },
    to: "/w/$slug/campaigns/$campaignId",
  },
  primary: c.name,
  secondary: planLine(c),
  trailing: <StatusBadge kind="campaign" value={c.status} />,
});

/** A conversation in the short list: who, about what, how recent, and "New" (pink) when unread. */
const conversationRow = (t: ThreadObject, slug: string): ListRow => ({
  key: t.id,
  link: {
    params: { slug, threadId: t.id },
    to: "/w/$slug/inbox/$threadId",
  },
  primary: t.participants[0] ?? t.subject ?? "Conversation",
  secondary: t.subject ?? undefined,
  trailing: (
    <span className="text-fg-3 flex shrink-0 items-center gap-2 text-xs">
      {t.unread ? <span className="text-accent font-semibold">New</span> : null}
      <RelativeTime value={t.last_activity_at} />
    </span>
  ),
});

/**
 * The last 30 days: the four figures once the report has counted (one sentence before, instead of
 * four "Pending"), and the daily chart once something was sent (never an empty chart).
 */
const Figures = ({
  analytics,
}: {
  analytics: UseQueryResult<AnalyticsObject>;
}) => {
  const report = analytics.data;
  if (!report?.computed_at) {
    return (
      <p className="text-fg-3 text-sm">
        Figures for the last 30 days appear a few minutes after the first emails
        go out.
      </p>
    );
  }
  return (
    <>
      <MetricGroup
        footer="The last 30 days. Sent means the recipient's server accepted the email."
        metrics={keyMetrics(report)}
      />
      {report.totals.sent > 0 ? <ActivityCard analytics={analytics} /> : null}
    </>
  );
};

/**
 * A workspace's front page, the first thing a person sees. Until it can send: the next setup
 * step, one at a time. Then what is happening: the last 30 days (once counted, said once before),
 * the daily chart once something was sent, the latest conversations first (replies are what
 * campaigns are for), then the campaigns with their plan and the mailboxes with today's sending.
 */
const Overview = () => {
  const workspace = useWorkspace();
  const link = { params: { slug: workspace.slug } };
  const details = useQuery(workspaceQuery(workspace));
  const mailboxes = useQuery(mailboxOptionsQuery(workspace));
  const campaigns = useQuery(campaignOptionsQuery(workspace));
  const conversations = useLatestConversations(workspace);
  const analytics = useAnalytics(workspace);

  const mailboxList = mailboxes.data?.data ?? [];
  const campaignList = campaigns.data?.data ?? [];
  const people = details.data?.usage?.people.used ?? 0;
  const launched = campaignList.some(
    (c) => c.status !== "draft" && c.status !== "archived"
  );
  const steps = setupSteps(workspace.slug, {
    launched,
    mailbox: mailboxList.length > 0,
    people: people > 0,
  });
  const loaded =
    mailboxes.isSuccess && campaigns.isSuccess && details.isSuccess;
  const settingUp = loaded && steps.some((step) => !step.done);

  const mailboxRows = mailboxList
    .slice(0, FIRST)
    .map((m) => mailboxRow(m, workspace.slug));
  const campaignRows = campaignList
    .toSorted(
      (a, b) =>
        CAMPAIGN_ORDER.indexOf(a.status) - CAMPAIGN_ORDER.indexOf(b.status)
    )
    .slice(0, FIRST)
    .map((c) => campaignRow(c, workspace.slug));
  const conversationRows = (conversations.data?.data ?? []).map((t) =>
    conversationRow(t, workspace.slug)
  );

  return (
    <PageBody>
      <PageHeader
        actions={
          settingUp ? null : (
            <Button
              render={
                <Link
                  params={{ slug: workspace.slug }}
                  search={{ new: true }}
                  to="/w/$slug/campaigns"
                />
              }
              variant="primary"
            >
              <HugeiconsIcon icon={Add01Icon} />
              New campaign
            </Button>
          )
        }
        compact
        title="Overview"
      />
      {settingUp ? (
        <div className="flex flex-col gap-4">
          <GettingStarted steps={steps} />
          <p className="text-fg-3 text-sm">
            Working from code instead?{" "}
            <Link
              className="text-fg-2 hover:text-fg underline underline-offset-2 transition-colors"
              params={{ slug: workspace.slug }}
              to="/w/$slug/api-keys"
            >
              Connect with the API
            </Link>
            : every step here has an endpoint.
          </p>
        </div>
      ) : (
        <div className="flex flex-col gap-4">
          <Figures analytics={analytics} />
          <div className="grid items-start gap-4 lg:grid-cols-2">
            <ListCard
              all={{ ...link, to: "/w/$slug/inbox" }}
              empty={
                <div className="flex items-center gap-4">
                  <Illustration
                    className="w-[112px]"
                    name="inbox"
                    once="nb.drawn.inbox"
                  />
                  <span>
                    When someone replies, the conversation lands here, newest
                    first.
                  </span>
                </div>
              }
              loading={conversations.isPending}
              rows={conversationRows}
              title="Conversations"
            />
            <div className="flex min-w-0 flex-col gap-4">
              <ListCard
                all={{ ...link, to: "/w/$slug/campaigns" }}
                empty="No campaign yet."
                loading={campaigns.isPending}
                rows={campaignRows}
                title="Campaigns"
              />
              <ListCard
                all={{ ...link, to: "/w/$slug/mailboxes" }}
                empty="No mailbox connected yet."
                loading={mailboxes.isPending}
                rows={mailboxRows}
                title="Mailboxes"
              />
            </div>
          </div>
        </div>
      )}
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/")({
  head: () => ({ meta: [{ title: "Overview · Norbelys" }] }),
  component: Overview,
});
