import type { CampaignObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";

import { campaignAnalyticsQuery } from "@/features/campaigns/queries";
import { ActivityCard } from "@/features/overview/activity-chart";
import { useWorkspace } from "@/lib/workspace";

/** The campaign's messages sent per UTC day over the last 30 days, from the analytics rollup. */
export const CampaignActivity = ({
  campaign,
}: {
  campaign: CampaignObject;
}) => {
  const workspace = useWorkspace();
  const analytics = useQuery(
    campaignAnalyticsQuery(workspace, campaign, "day")
  );
  return <ActivityCard analytics={analytics} />;
};
