import { RefreshIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { AnalyticsObject, Counters } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { createStandardSchemaV1, useQueryStates } from "nuqs";
import { useEffect } from "react";
import type * as React from "react";

import { Dash, DataTable, EmptyPanel } from "@/components/data-table";
import { DateRangeControl } from "@/components/date-range-control";
import { PageBody, PageHeader, Section } from "@/components/page";
import { Problem } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  ActivityChart,
  COUNTER_LABELS,
  WAITING_FOR_REPORT,
} from "@/features/overview/activity-chart";
import { calendarDay, rangeProblem, recentRange } from "@/lib/date-range";
import type { DateRange } from "@/lib/date-range";
import { formatCount, formatRate, formatTimestamp } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

const search = { from: calendarDay, to: calendarDay };
const MAX_RANGE_DAYS = 366;

/** The counters, in the order the page draws them, with the words that keep them honest. */
const METRICS: { key: keyof Counters; hint: string }[] = [
  { hint: "Accepted by the recipient's server", key: "sent" },
  { hint: "Reported delivered", key: "delivered" },
  { hint: "Hard and soft, as reported", key: "bounced" },
  { hint: "Replies classified as human", key: "replied" },
  { hint: "Scanners and proxies left out", key: "opened" },
  { hint: "Scanners and proxies left out", key: "clicked" },
  { hint: "Spam reports from feedback loops", key: "complained" },
  { hint: "One-click and link unsubscribes", key: "unsubscribed" },
];

const useCounters = (
  workspace: Workspace,
  range: DateRange,
  groupBy: "day" | "campaign"
) => {
  const { from, to } = range;
  return useQuery({
    enabled: !rangeProblem(range, MAX_RANGE_DAYS),
    queryFn: ({ signal }) =>
      workspace.api.analytics.retrieve(
        { from, group_by: groupBy, to },
        { signal }
      ),
    queryKey: [workspace.id, "analytics", groupBy, from, to],
  });
};

/** One counter's card: its total, what it means, and its bars per day. */
const MetricCard = ({
  hint,
  metric,
  report,
}: {
  hint: string;
  metric: keyof Counters;
  report: AnalyticsObject;
}) => (
  <section className="border-line bg-surface flex min-w-0 flex-col gap-2 rounded-[8px] border p-6">
    <header className="flex items-baseline justify-between gap-3">
      <h2 className="text-fg text-base font-medium">
        {COUNTER_LABELS[metric]}
      </h2>
      <span className="text-fg text-xl font-semibold tabular-nums">
        {formatCount(report.totals[metric])}
      </span>
    </header>
    <p className="text-fg-3 text-xs">{hint}</p>
    <ActivityChart
      data={report.data}
      from={report.from}
      height={160}
      metric={metric}
      to={report.to}
    />
  </section>
);

interface CampaignRow {
  campaignId: string;
  counters: Counters;
}

/** The range's counters per campaign, with rates only where something was sent. */
const ByCampaign = ({ report }: { report: AnalyticsObject }) => {
  const workspace = useWorkspace();
  const rows: CampaignRow[] = report.data.flatMap((group) =>
    group.campaign_id
      ? [{ campaignId: group.campaign_id, counters: group.counters }]
      : []
  );
  return (
    <DataTable<CampaignRow>
      columns={[
        {
          header: "Campaign",
          id: "campaign",
          render: (row) => (
            <Link
              className="text-link hover:text-link-hover font-mono text-xs"
              params={{ campaignId: row.campaignId, slug: workspace.slug }}
              to="/w/$slug/campaigns/$campaignId"
            >
              {row.campaignId}
            </Link>
          ),
        },
        {
          header: "Sent",
          id: "sent",
          render: (row) => formatCount(row.counters.sent),
        },
        {
          header: "Delivered",
          id: "delivered",
          render: (row) => formatCount(row.counters.delivered),
        },
        {
          header: "Reply rate",
          id: "replied",
          render: (row) =>
            formatRate(row.counters.replied, row.counters.sent) ?? <Dash />,
        },
        {
          header: "Bounce rate",
          id: "bounced",
          render: (row) =>
            formatRate(row.counters.bounced, row.counters.sent) ?? <Dash />,
        },
        {
          header: "Unsubscribes",
          id: "unsubscribed",
          render: (row) => formatCount(row.counters.unsubscribed),
        },
      ]}
      empty={{
        description: "No campaign sent anything in this range.",
        icon: RefreshIcon,
        illustration: "reports",
        title: "Nothing to compare yet",
      }}
      rowKey={(row) => row.campaignId}
      rows={rows}
    />
  );
};

/** The per-campaign table, a placeholder while it loads, or why there is none yet. */
const CampaignSection = ({
  report,
}: {
  report: AnalyticsObject | undefined;
}) => {
  if (!report) {
    return <Skeleton className="h-40" />;
  }
  if (!report.computed_at) {
    return <p className="text-fg-3 text-sm">Waiting for the first report.</p>;
  }
  return <ByCampaign report={report} />;
};

const Charts = ({ range }: { range: DateRange }) => {
  const workspace = useWorkspace();
  const daily = useCounters(workspace, range, "day");
  if (daily.isError) {
    return (
      <Problem
        error={daily.error}
        onRetry={() => {
          void daily.refetch();
        }}
      />
    );
  }
  if (!daily.data) {
    return (
      <div className="grid gap-3 lg:grid-cols-2 2xl:grid-cols-4">
        {METRICS.map((metric) => (
          <Skeleton className="h-[260px] rounded-[8px]" key={metric.key} />
        ))}
      </div>
    );
  }
  if (!daily.data.computed_at) {
    return (
      <p className="border-line text-fg-3 grid h-60 place-items-center rounded-[8px] border text-sm">
        {WAITING_FOR_REPORT}
      </p>
    );
  }
  const report = daily.data;
  return (
    <div className="grid gap-3 lg:grid-cols-2 2xl:grid-cols-4">
      {METRICS.map((metric) => (
        <MetricCard
          hint={metric.hint}
          key={metric.key}
          metric={metric.key}
          report={report}
        />
      ))}
    </div>
  );
};

/**
 * The workspace's counters over a range, as the console's monitoring: a range control, a card per
 * counter with its bars per day, and the same range per campaign. Counters come from the rollup,
 * a few minutes behind; a missing report is said, never drawn as zero.
 */
const Analytics = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [dates, setDates] = useQueryStates(search, { history: "push" });
  const defaults = recentRange(30, dates.to ? Date.parse(dates.to) : undefined);
  const range = {
    from: dates.from ?? defaults.from,
    to: dates.to ?? defaults.to,
  };
  useEffect(() => {
    if (!dates.from || !dates.to) {
      void setDates({ from: range.from, to: range.to }, { history: "replace" });
    }
  }, [dates.from, dates.to, range.from, range.to, setDates]);
  const invalid = rangeProblem(range, MAX_RANGE_DAYS);
  const byCampaign = useCounters(workspace, range, "campaign");
  const counted = byCampaign.data?.computed_at;
  let content: React.ReactNode;
  if (invalid) {
    content = (
      <p className="text-error text-sm" role="alert">
        {invalid}
      </p>
    );
  } else if (byCampaign.data && !counted) {
    content = (
      <EmptyPanel
        description="Sent, delivered, replies and bounces, per day and per campaign, a few minutes after your first emails go out."
        icon={RefreshIcon}
        illustration="reports"
        title="Your figures appear here"
      />
    );
  } else {
    content = (
      <>
        <Charts range={range} />
        <Section title="By campaign">
          {byCampaign.isError ? (
            <Problem
              error={byCampaign.error}
              onRetry={() => {
                void byCampaign.refetch();
              }}
            />
          ) : (
            <CampaignSection report={byCampaign.data} />
          )}
        </Section>
      </>
    );
  }
  return (
    <PageBody>
      <PageHeader title="Analytics" />
      <div className="flex flex-col gap-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <DateRangeControl
            maxDays={MAX_RANGE_DAYS}
            key={`${range.from}.${range.to}`}
            onChange={(next) => {
              void setDates(next);
            }}
            range={range}
          />
          <div className="flex flex-wrap items-center gap-3">
            {counted ? (
              <span className="text-fg-3 text-xs">
                Counted through {formatTimestamp(counted)}
              </span>
            ) : null}
            <Button
              onClick={() => {
                void queryClient.invalidateQueries({
                  queryKey: [workspace.id, "analytics"],
                });
              }}
              variant="secondary"
            >
              <HugeiconsIcon icon={RefreshIcon} />
              Refresh
            </Button>
          </div>
        </div>
        {content}
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/analytics")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Analytics · Norbelys" }] }),
  component: Analytics,
});
