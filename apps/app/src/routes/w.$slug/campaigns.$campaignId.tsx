import { useSuspenseQuery } from "@tanstack/react-query";
import {
  createFileRoute,
  Outlet,
  useRouterState,
} from "@tanstack/react-router";

import { PageBody } from "@/components/page";
import { CampaignHeader } from "@/features/campaigns/campaign-header";
import { campaignQuery } from "@/features/campaigns/queries";
import { useWorkspace } from "@/lib/workspace";

/** One campaign: its header, actions and tabs over the section the address names. */
const CampaignLayout = () => {
  const workspace = useWorkspace();
  const { campaignId } = Route.useParams();
  const sequence = useRouterState({
    select: (state) => state.location.pathname.endsWith("/sequence"),
  });
  const { data: campaign } = useSuspenseQuery(
    campaignQuery(workspace, campaignId)
  );
  return (
    <PageBody className={sequence ? "lg:pb-0" : undefined}>
      <CampaignHeader campaign={campaign} />
      <Outlet />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/campaigns/$campaignId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      campaignQuery(context.workspace, params.campaignId)
    ),
  head: ({ loaderData }) => ({
    meta: [{ title: `${loaderData?.name ?? "Campaign"} · Norbelys` }],
  }),
  component: CampaignLayout,
});
