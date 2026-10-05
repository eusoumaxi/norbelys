import { createFileRoute } from "@tanstack/react-router";

import { campaignSectionHead } from "@/features/campaigns/format";
import { CampaignSettings } from "@/features/campaigns/settings/campaign-settings";

export const Route = createFileRoute("/w/$slug/campaigns/$campaignId/settings")(
  {
    head: ({ matches }) => campaignSectionHead(matches, "Settings"),
    component: CampaignSettings,
  }
);
