import { createFileRoute } from "@tanstack/react-router";

import { CampaignMessages } from "@/features/campaigns/campaign-messages";
import { campaignSectionHead } from "@/features/campaigns/format";

export const Route = createFileRoute("/w/$slug/campaigns/$campaignId/messages")(
  {
    head: ({ matches }) => campaignSectionHead(matches, "Messages"),
    component: CampaignMessages,
  }
);
