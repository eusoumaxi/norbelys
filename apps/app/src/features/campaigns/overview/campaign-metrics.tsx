import type { CampaignObject } from "@norbelys/sdk";

import { MetricGroup } from "@/components/details";
import type { Metric } from "@/components/details";
import { formatCount, formatRate, formatRelative } from "@/lib/format";

/**
 * The campaign's figures over its whole history, once the report has counted something: sent,
 * then what came of it with its share of sent. Replies come first after sent, in pink, because
 * they are what a campaign is for; opens and clicks only appear when the campaign tracks them.
 */
export const CampaignMetrics = ({ campaign }: { campaign: CampaignObject }) => {
  const { stats, tracking } = campaign;
  const share = (value: number) => formatRate(value, stats.sent) ?? undefined;
  const metrics: Metric[] = [
    { label: "Sent", value: formatCount(stats.sent) },
    {
      accent: stats.replied > 0,
      label: "Replies",
      note: share(stats.replied),
      value: formatCount(stats.replied),
    },
    tracking.opens
      ? {
          label: "Opened",
          note: share(stats.opened),
          value: formatCount(stats.opened),
        }
      : null,
    tracking.clicks
      ? {
          label: "Clicked",
          note: share(stats.clicked),
          value: formatCount(stats.clicked),
        }
      : null,
    {
      label: "Bounced",
      note: share(stats.bounced),
      value: formatCount(stats.bounced),
    },
    {
      label: "Unsubscribed",
      note: share(stats.unsubscribed),
      value: formatCount(stats.unsubscribed),
    },
  ].filter((metric) => metric !== null);
  return (
    <MetricGroup
      footer={
        stats.computed_at
          ? `Counted ${formatRelative(stats.computed_at).toLowerCase()}. Sent means the recipient's server accepted the email; opens and clicks count people, not link scanners.`
          : undefined
      }
      metrics={metrics}
    />
  );
};
