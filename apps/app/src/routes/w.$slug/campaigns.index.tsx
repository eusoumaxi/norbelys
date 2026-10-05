import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { z } from "zod";

import { CampaignList } from "@/features/campaigns/campaign-list";
import { useWorkspace } from "@/lib/workspace";

/**
 * `/w/$slug/campaigns`: the workspace's campaigns. `?new=true` opens the "New campaign" dialog,
 * so other pages can link straight to it; each campaign is its own page beside this one.
 */
const CampaignsPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const { new: creating } = Route.useSearch();
  return (
    <CampaignList
      creating={Boolean(creating)}
      onCreatingChange={(open) => {
        void navigate({
          params: { slug: workspace.slug },
          replace: true,
          search: open ? { new: true } : {},
          to: "/w/$slug/campaigns",
        });
      }}
    />
  );
};

export const Route = createFileRoute("/w/$slug/campaigns/")({
  validateSearch: z.object({ new: z.boolean().optional() }),
  head: () => ({ meta: [{ title: "Campaigns · Norbelys" }] }),
  component: CampaignsPage,
});
