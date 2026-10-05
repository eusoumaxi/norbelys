import {
  Add01Icon,
  Megaphone01Icon,
  Search01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { CampaignObject, CampaignStatus } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { useDeferredValue, useState } from "react";
import { toast } from "sonner";

import { CreateDialog } from "@/components/create-dialog";
import { Dash, ListTable } from "@/components/data-table";
import type { Column } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Segmented } from "@/components/ui/segmented";
import type { SegmentedOption } from "@/components/ui/segmented";
import { useRunControl } from "@/features/campaigns/campaign-actions";
import { planLine } from "@/features/campaigns/format";
import {
  campaignListQuery,
  campaignOptionsQuery,
  campaignsKey,
} from "@/features/campaigns/queries";
import { formatCount, formatRate, formatRelative } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

type Filter = "all" | CampaignStatus;

const FILTERS: SegmentedOption<Filter>[] = [
  { label: "All", value: "all" },
  { label: "Draft", value: "draft" },
  { label: "Active", value: "active" },
  { label: "Paused", value: "paused" },
  { label: "Completed", value: "completed" },
  { label: "Archived", value: "archived" },
];

/** A rate of what was sent, or nothing before the report has counted (Sent says "Pending"). */
const Rate = ({
  campaign,
  part,
}: {
  campaign: CampaignObject;
  part: number;
}) =>
  campaign.stats.computed_at ? (
    (formatRate(part, campaign.stats.sent) ?? <Dash />)
  ) : (
    <Dash />
  );

/**
 * The `⋯` menu of a campaign row: pause or resume it, or copy its id. Starting is left to the
 * campaign's page, where the person sees who gets what before approving it.
 */
const CampaignRowMenu = ({ campaign }: { campaign: CampaignObject }) => {
  const control = useRunControl(campaign);
  return (
    <RowMenu>
      {control && campaign.status !== "draft" ? (
        <DropdownMenuItem onClick={control.handleRun}>
          {control.label}
        </DropdownMenuItem>
      ) : null}
      <CopyIdItem id={campaign.id} noun="campaign" />
    </RowMenu>
  );
};

const COLUMNS: Column<CampaignObject>[] = [
  {
    render: (c) => (
      <span className="flex min-w-0 flex-col">
        <span className="text-fg truncate font-semibold">{c.name}</span>
        <span className="text-fg-3 truncate text-xs">{planLine(c)}</span>
      </span>
    ),
    header: "Name",
    id: "name",
  },
  {
    render: (c) => <StatusBadge kind="campaign" value={c.status} />,
    header: "Status",
    id: "status",
  },
  {
    render: (c) =>
      c.stats.computed_at ? (
        <span className="tabular-nums">{formatCount(c.stats.sent)}</span>
      ) : (
        <span className="text-fg-3">Pending</span>
      ),
    header: "Sent",
    id: "sent",
  },
  {
    render: (c) =>
      c.stats.computed_at && c.stats.replied > 0 ? (
        <span className="tabular-nums">
          <span className="text-accent font-semibold">
            {formatCount(c.stats.replied)}
          </span>
          <span className="text-fg-3 ml-1.5">
            {formatRate(c.stats.replied, c.stats.sent)}
          </span>
        </span>
      ) : (
        <Rate campaign={c} part={c.stats.replied} />
      ),
    header: "Replies",
    id: "replies",
  },
  {
    render: (c) => <Rate campaign={c} part={c.stats.bounced} />,
    header: "Bounced",
    id: "bounces",
  },
  {
    render: (c) => (
      <span className="text-fg-3">{formatRelative(c.updated_at)}</span>
    ),
    header: "Updated",
    id: "updated",
  },
  {
    render: (c) => <CampaignRowMenu campaign={c} />,
    className: "w-[62px]",
    header: "",
    id: "menu",
  },
];

/**
 * The workspace's campaigns: filtered by status and searched by the start of their names (what
 * `campaigns.list` offers), each row opening the campaign with its plan under its name and its
 * replies in pink. "New campaign" creates a draft and
 * opens its sequence, the first thing a new campaign needs.
 */
export const CampaignList = ({
  creating,
  onCreatingChange,
}: {
  /** Whether the "New campaign" dialog is open (it lives in the URL, so links can open it). */
  creating: boolean;
  onCreatingChange: (open: boolean) => void;
}) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [filter, setFilter] = useState<Filter>("all");
  const [input, setInput] = useState("");
  const q = useDeferredValue(input.trim());
  const status = filter === "all" ? undefined : filter;
  const searching = Boolean(q) || status !== undefined;
  // Search and filters wait until there is a campaign to find.
  const options = useQuery(campaignOptionsQuery(workspace));
  const none = !searching && options.data?.data.length === 0;

  const newButton = (
    <Button onClick={() => onCreatingChange(true)} variant="primary">
      <HugeiconsIcon icon={Add01Icon} />
      New campaign
    </Button>
  );

  return (
    <PageBody>
      <PageHeader actions={newButton} title="Campaigns" />
      <div className="flex flex-col gap-2">
        <div
          className={cn(
            "flex flex-col gap-2 lg:flex-row lg:items-center",
            none && "hidden"
          )}
        >
          <SearchInput
            className="lg:flex-1"
            label="Search campaigns"
            maxLength={200}
            onChange={setInput}
            placeholder="Search campaigns by name"
            value={input}
          />
          <Segmented
            className="overflow-x-auto"
            label="Status"
            onChange={setFilter}
            options={FILTERS}
            value={filter}
          />
        </div>
        <ListTable<CampaignObject>
          columns={COLUMNS}
          empty={
            searching
              ? {
                  description:
                    "No campaign matches this status and search. Clear them to see every campaign.",
                  icon: Search01Icon,
                  title: "No matching campaigns",
                }
              : {
                  action: newButton,
                  description:
                    "A campaign sends a sequence of steps to the people you enroll, from your mailboxes, within their limits.",
                  icon: Megaphone01Icon,
                  illustration: "campaign",
                  title: "No campaigns yet",
                }
          }
          onRowClick={(campaign) => {
            void navigate({
              params: { campaignId: campaign.id, slug: workspace.slug },
              to: "/w/$slug/campaigns/$campaignId",
            });
          }}
          query={campaignListQuery(workspace, { q, status })}
          rowKey={(c) => c.id}
        />
      </div>
      <CreateDialog
        description="It starts as a draft: write its sequence, then choose its senders and schedule before starting it."
        fields={[
          {
            label: "Name",
            name: "name",
            placeholder: "Q4 founders outreach",
            required: true,
          },
        ]}
        onOpenChange={onCreatingChange}
        onSubmit={async (values) => {
          const campaign = await workspace.api.campaigns.create({
            name: values.name ?? "",
          });
          toast.success("Campaign created as a draft");
          await queryClient.invalidateQueries({
            queryKey: campaignsKey(workspace),
          });
          await navigate({
            params: { campaignId: campaign.id, slug: workspace.slug },
            to: "/w/$slug/campaigns/$campaignId/sequence",
          });
        }}
        open={creating}
        submitLabel="Create campaign"
        title="New campaign"
      />
    </PageBody>
  );
};
