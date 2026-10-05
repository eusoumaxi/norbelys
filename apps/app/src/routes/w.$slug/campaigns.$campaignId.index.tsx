import { createFileRoute } from "@tanstack/react-router";

import { CampaignOverview } from "@/features/campaigns/overview/campaign-overview";

export const Route = createFileRoute("/w/$slug/campaigns/$campaignId/")({
  component: CampaignOverview,
});
