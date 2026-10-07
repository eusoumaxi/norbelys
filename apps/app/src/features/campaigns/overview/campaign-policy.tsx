import type { CampaignObject, PolicyCounts, PolicyGroup } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { MetricGroup } from "@/components/details";
import { Problem } from "@/components/problem";
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
import { campaignPolicyQuery } from "@/features/campaigns/queries";
import { formatCount, formatRelative, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

const columns: (readonly [keyof PolicyCounts, string])[] = [
  ["affected", "Affected"],
  ["pending", "Awaiting outcome"],
  ["recovered", "Delivery confirmed"],
  ["failed", "Final refusal"],
  ["account_restricted", "Account restriction"],
];

/** Frozen variant versions retain their own row even after a campaign's content changes. */
const label = (campaign: CampaignObject, group: PolicyGroup): string => {
  const step = campaign.steps.find((item) => item.id === group.step_id);
  const stepLabel = step
    ? `${step.position}. ${step.name}`
    : shortId(group.step_id ?? "");
  if (!group.variant_id) {
    return stepLabel;
  }
  const variant = step?.variants.find((item) => item.id === group.variant_id);
  return `${stepLabel} · ${variant?.name ?? shortId(group.variant_id)} · v${group.variant_version ?? 0}`;
};

/** Show business delivery failures even when submission succeeded and runtime logs are healthy. */
export const CampaignPolicy = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const [grouping, setGrouping] = useState<"step" | "variant">("variant");
  const query = useQuery(campaignPolicyQuery(workspace, campaign, grouping));
  const report = query.data?.policy;
  return (
    <Card>
      <CardHeader>
        <div className="flex flex-col gap-0.5">
          <CardTitle>Delivery blocks</CardTitle>
          <CardDescription className="text-xs">
            Distinct messages with a provider policy refusal. Repeated notices
            count once; affected includes messages later delivered.
          </CardDescription>
        </div>
        <Segmented
          label="Group delivery blocks"
          onChange={setGrouping}
          options={[
            { label: "By step", value: "step" },
            { label: "By variant", value: "variant" },
          ]}
          value={grouping}
        />
      </CardHeader>
      <CardContent className="space-y-4">
        {query.isError ? (
          <Problem
            error={query.error}
            onRetry={() => {
              void query.refetch();
            }}
          />
        ) : null}
        {query.isPending ? <Skeleton className="h-24" /> : null}
        {report ? (
          <>
            <MetricGroup
              footer={`Evidence checked ${formatRelative(report.computed_at).toLowerCase()}. Based on retained message history; archived evidence is excluded.`}
              metrics={columns.map(([key, name]) => ({
                label: name,
                value: formatCount(report.totals[key]),
              }))}
            />
            <p className="text-fg-2 text-sm">
              Account restrictions affect the sending route, so they do not
              prove that a variant caused the block. Other policy refusals have
              unknown scope; check the provider response in the message’s
              delivery history before comparing content. Awaiting outcome is
              neither a confirmed delivery nor a final bounce.
            </p>
            {report.totals.account_restricted > 0 ? (
              <p className="text-warning text-sm font-semibold">
                The provider reported JFE050005: an account restriction
                associated with content-policy violations. Check the sending
                account before interpreting variant performance.
              </p>
            ) : null}
            {report.data.length > 0 ? (
              <div className="overflow-x-auto">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>
                        {grouping === "variant" ? "Variant" : "Step"}
                      </TableHead>
                      {columns.map(([key, name]) => (
                        <TableHead key={key}>{name}</TableHead>
                      ))}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {report.data.map((group) => (
                      <TableRow
                        key={`${group.step_id}:${group.variant_id}:${group.variant_version}`}
                      >
                        <TableCell>{label(campaign, group)}</TableCell>
                        {columns.map(([key]) => (
                          <TableCell className="tabular-nums" key={key}>
                            {formatCount(group.counts[key])}
                          </TableCell>
                        ))}
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            ) : (
              <p className="text-fg-3 text-sm">
                No policy refusals in retained delivery history.
              </p>
            )}
            {report.has_more ? (
              <p className="text-fg-3 text-xs">
                Only the first 2,500 groups are shown; totals include all
                groups.
              </p>
            ) : null}
          </>
        ) : null}
        {!query.isPending && !query.isError && !report ? (
          <p className="text-fg-3 text-sm">
            Policy evidence is not available from this server version.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
};
