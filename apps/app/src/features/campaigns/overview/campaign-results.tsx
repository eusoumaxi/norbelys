import type {
  AnalyticsGroup,
  AnalyticsObject,
  CampaignObject,
  Counters,
} from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import type { UseQueryResult } from "@tanstack/react-query";
import { cn } from "cn";
import { useState } from "react";

import { Problem } from "@/components/problem";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Segmented } from "@/components/ui/segmented";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { campaignAnalyticsQuery, lifetime } from "@/features/campaigns/queries";
import { WAITING_FOR_REPORT } from "@/features/overview/activity-chart";
import { formatCount, formatDate, formatRate, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

type Grouping = "step" | "variant";

/** One line of the results table: who it is about, and its counters. */
interface ResultRow {
  key: string;
  step: string;
  variant?: string;
  winner?: boolean;
  counters: Counters;
}

const ZERO: Counters = {
  bounced: 0,
  clicked: 0,
  complained: 0,
  delivered: 0,
  opened: 0,
  replied: 0,
  sent: 0,
  unsubscribed: 0,
};

/** One line per current step, in order; a step the report has not seen sent nothing. */
const stepRows = (
  campaign: CampaignObject,
  groups: AnalyticsGroup[]
): ResultRow[] =>
  campaign.steps.map((step) => ({
    counters: groups.find((g) => g.step_id === step.id)?.counters ?? ZERO,
    key: step.id,
    step: `${step.position}. ${step.name}`,
  }));

/**
 * One line per variant version the report counted, by step: each published version of a
 * variant's content counts apart, and a variant no longer offered keeps its line.
 */
const variantRows = (
  campaign: CampaignObject,
  groups: AnalyticsGroup[]
): ResultRow[] =>
  groups.map((group) => {
    const step = campaign.steps.find((s) => s.id === group.step_id);
    const variant = step?.variants.find((v) => v.id === group.variant_id);
    const version = group.variant_version ? ` · v${group.variant_version}` : "";
    return {
      counters: group.counters,
      key: `${group.variant_id ?? ""}:${group.variant_version ?? 0}`,
      step: step
        ? `${step.position}. ${step.name}`
        : shortId(group.step_id ?? ""),
      variant: `${variant?.name ?? shortId(group.variant_id ?? "")}${version}`,
      winner: Boolean(
        step?.winner && step.winner.variant_id === group.variant_id
      ),
    };
  });

/** A count with its share of sent under it; replies, once there are some, in pink. */
const Count = ({
  accent = false,
  sent,
  value,
}: {
  accent?: boolean;
  sent: number;
  value: number;
}) => {
  const share = formatRate(value, sent);
  return (
    <span className="flex flex-col">
      <span
        className={cn(
          "tabular-nums",
          accent && value > 0 && "text-accent font-semibold"
        )}
      >
        {formatCount(value)}
      </span>
      {share ? <span className="text-fg-3 text-xs">{share}</span> : null}
    </span>
  );
};

/** The columns after Sent: replies first; opens and clicks only when the campaign tracks them. */
const counters = (
  campaign: CampaignObject
): (readonly [keyof Counters, string])[] =>
  [
    ["replied", "Replies"] as const,
    ["delivered", "Delivered"] as const,
    campaign.tracking.opens ? (["opened", "Opened"] as const) : null,
    campaign.tracking.clicks ? (["clicked", "Clicked"] as const) : null,
    ["bounced", "Bounced"] as const,
    ["unsubscribed", "Unsubscribed"] as const,
  ].filter((column) => column !== null);

/** The table of one grouping, or why there is none yet. */
const ResultsTable = ({
  campaign,
  grouping,
  report,
}: {
  campaign: CampaignObject;
  grouping: Grouping;
  report: UseQueryResult<AnalyticsObject>;
}) => {
  if (report.isError) {
    return (
      <Problem
        error={report.error}
        onRetry={() => {
          void report.refetch();
        }}
      />
    );
  }
  if (!report.data) {
    return <Skeleton className="mx-4 mb-4 h-24" />;
  }
  if (!report.data.computed_at) {
    return <p className="text-fg-3 px-4 pb-6 text-sm">{WAITING_FOR_REPORT}</p>;
  }
  const columns = counters(campaign);
  const rows =
    grouping === "step"
      ? stepRows(campaign, report.data.data)
      : variantRows(campaign, report.data.data);
  if (rows.length === 0) {
    return (
      <p className="text-fg-3 px-4 pb-6 text-sm">
        Nothing was sent yet, so there is nothing to compare.
      </p>
    );
  }
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Step</TableHead>
          {grouping === "variant" ? <TableHead>Variant</TableHead> : null}
          <TableHead>Sent</TableHead>
          {columns.map(([key, label]) => (
            <TableHead key={key}>{label}</TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((row) => (
          <TableRow key={row.key}>
            <TableCell className="text-fg font-semibold">{row.step}</TableCell>
            {grouping === "variant" ? (
              <TableCell>
                <span className="flex items-center gap-2">
                  {row.variant}
                  {row.winner ? <Badge tone="accent">Winner</Badge> : null}
                </span>
              </TableCell>
            ) : null}
            <TableCell className="tabular-nums">
              {formatCount(row.counters.sent)}
            </TableCell>
            {columns.map(([key]) => (
              <TableCell key={key}>
                <Count
                  accent={key === "replied"}
                  sent={row.counters.sent}
                  value={row.counters[key]}
                />
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
};

/**
 * How each step, and each variant of each step, did over the campaign's whole history (as far as
 * one report reaches: a year), so a sequence's weak step or a test's better variant stands out.
 */
export const CampaignResults = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const [grouping, setGrouping] = useState<Grouping>("step");
  const report = useQuery(
    campaignAnalyticsQuery(workspace, campaign, grouping)
  );
  const range = lifetime(campaign);
  return (
    <Card>
      <CardHeader>
        <div className="flex flex-col gap-0.5">
          <CardTitle>Step by step</CardTitle>
          <CardDescription className="text-xs">
            Since {formatDate(range.from)}. Percentages are of the emails each
            step sent.
          </CardDescription>
        </div>
        <Segmented
          label="Group results"
          onChange={setGrouping}
          options={[
            { label: "By step", value: "step" },
            { label: "By variant", value: "variant" },
          ]}
          value={grouping}
        />
      </CardHeader>
      <CardContent className="overflow-x-auto px-0 pb-0">
        <ResultsTable campaign={campaign} grouping={grouping} report={report} />
        {report.data?.has_more ? (
          <p className="text-fg-3 border-line border-t px-4 py-3 text-xs">
            Only the first 2,500 groups are shown.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
};
