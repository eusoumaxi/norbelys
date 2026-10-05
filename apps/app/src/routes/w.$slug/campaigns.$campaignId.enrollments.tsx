import { createFileRoute } from "@tanstack/react-router";

import { CampaignEnrollments } from "@/features/campaigns/enrollments/campaign-enrollments";
import { campaignSectionHead } from "@/features/campaigns/format";

export const Route = createFileRoute(
  "/w/$slug/campaigns/$campaignId/enrollments"
)({
  head: ({ matches }) => campaignSectionHead(matches, "Enrollments"),
  component: CampaignEnrollments,
});
