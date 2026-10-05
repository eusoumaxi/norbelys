import type { CampaignObject } from "@norbelys/sdk";
import { cn } from "cn";

import { PageHeader } from "@/components/page";
import { StatusDot, statusText } from "@/components/ui/badge";
import { TabLinks } from "@/components/ui/tabs";
import type { TabLink } from "@/components/ui/tabs";
import { CampaignActions } from "@/features/campaigns/campaign-actions";
import { campaignNow, enrolled } from "@/features/campaigns/now";
import { enrollmentSummary } from "@/features/campaigns/queries";
import { useWorkspace } from "@/lib/workspace";

/**
 * The line under the campaign's name: its state beside a dot (which pings while it sends) and
 * what happens next, with the person's own clock beside the campaign's when they differ. It is
 * the one place the state is shown, so it replaces a status pill and an error banner.
 */
const StateLine = ({ campaign }: { campaign: CampaignObject }) => {
  const now = campaignNow(campaign, enrollmentSummary(campaign));
  const routine =
    now.tone === "success" || now.tone === "neutral" || now.tone === "info";
  return (
    <p className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-sm">
      <span
        className={cn(
          "inline-flex items-center gap-1.5 font-medium",
          routine ? "text-fg" : statusText[now.tone]
        )}
      >
        <StatusDot live={now.live} tone={now.tone} />
        {now.label}
      </span>
      <span className="text-fg-2">{now.sentence}</span>
      {now.local ? <span className="text-fg-3">({now.local})</span> : null}
    </p>
  );
};

/**
 * The top of every campaign page: the way back to the list, the name with its actions, the line
 * that says what it is doing now, and the tabs of its sections (the people enrolled counted
 * beside theirs).
 */
export const CampaignHeader = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const params = { campaignId: campaign.id, slug: workspace.slug };
  const summary = enrollmentSummary(campaign);
  const tabs: TabLink[] = [
    {
      exact: true,
      label: "Overview",
      link: { params, to: "/w/$slug/campaigns/$campaignId" },
    },
    {
      label: "Sequence",
      link: { params, to: "/w/$slug/campaigns/$campaignId/sequence" },
    },
    {
      count: summary ? enrolled(summary) : undefined,
      label: "Enrollments",
      link: { params, to: "/w/$slug/campaigns/$campaignId/enrollments" },
    },
    {
      label: "Messages",
      link: { params, to: "/w/$slug/campaigns/$campaignId/messages" },
    },
    {
      label: "Settings",
      link: { params, to: "/w/$slug/campaigns/$campaignId/settings" },
    },
  ];
  return (
    <>
      <PageHeader
        actions={<CampaignActions campaign={campaign} />}
        back={{
          label: "Campaigns",
          link: { params: { slug: workspace.slug }, to: "/w/$slug/campaigns" },
        }}
        compact
        subtitle={<StateLine campaign={campaign} />}
        title={campaign.name}
      />
      <TabLinks className="mb-5" tabs={tabs} />
    </>
  );
};
