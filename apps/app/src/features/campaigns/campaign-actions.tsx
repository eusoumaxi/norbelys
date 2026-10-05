import { PauseIcon, PlayIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { CampaignObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import type { ReactNode } from "react";
import { toast } from "sonner";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CreateDialog } from "@/components/create-dialog";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { Button } from "@/components/ui/button";
import {
  DropdownMenuItem,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { useUnsavedChanges } from "@/features/campaigns/campaign-editor";
import { identityLabel, poolOf } from "@/features/campaigns/pool";
import {
  campaignKey,
  campaignsKey,
  enrollmentSummary,
  hasMessagesQuery,
  sendersQuery,
} from "@/features/campaigns/queries";
import { useAction } from "@/lib/actions";
import {
  formatCount,
  formatWhenIn,
  formatWindow,
  plural,
  zoneName,
} from "@/lib/format";
import { describeProblem } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/**
 * What a campaign can do next, as its page and its row offer it: start a draft, resume a paused
 * campaign, or pause an active (or starting) one; `null` when it can do none of these.
 */
export const useRunControl = (campaign: CampaignObject) => {
  const workspace = useWorkspace();
  const action = useAction();
  const key = campaignsKey(workspace);
  if (campaign.status === "active" || campaign.status === "materialising") {
    return {
      handleRun: () =>
        action(
          "Campaign paused",
          () => workspace.api.campaigns.pause(campaign.id),
          key
        ),
      label: "Pause",
      starts: false,
    };
  }
  if (campaign.status === "draft" || campaign.status === "paused") {
    const draft = campaign.status === "draft";
    return {
      handleRun: () =>
        action(
          draft ? "Campaign started" : "Campaign resumed",
          () => workspace.api.campaigns.start(campaign.id),
          key
        ),
      label: draft ? "Start" : "Resume",
      starts: true,
    };
  }
  return null;
};

/** One line of the start summary: what, then its value. */
const Fact = ({ children, label }: { children: ReactNode; label: string }) => (
  <>
    <dt className="text-fg-3">{label}</dt>
    <dd className="text-fg min-w-0">{children}</dd>
  </>
);

/**
 * Starting is the moment the person approves sending, so it is never one click: the dialog says
 * who gets what, from which address, and when, in the campaign's own words, before anything goes
 * out. A missing piece (no step, no sender in its pool) is said here, not discovered later.
 */
const StartDialog = ({
  campaign,
  onConfirm,
  onOpenChange,
  open,
}: {
  campaign: CampaignObject;
  onConfirm: () => unknown;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const senders = useQuery({ ...sendersQuery(workspace), enabled: open });
  const unsaved = useUnsavedChanges(campaign.id);
  const summary = enrollmentSummary(campaign);
  const pool = senders.data ? poolOf(campaign.senders, senders.data) : null;
  const [first] = campaign.steps;
  const people = summary ? summary.active + summary.paused : null;
  const { schedule } = campaign;
  let from: ReactNode = "…";
  if (pool && pool.length === 0) {
    from = (
      <span className="text-warning">
        No address yet. Choose its mailboxes in Settings, or nothing can go out.
      </span>
    );
  } else if (pool) {
    const [only] = pool;
    from =
      pool.length === 1 && only
        ? identityLabel(only)
        : `${formatCount(pool.length)} addresses, taking turns`;
  }
  let who: ReactNode = "Everyone enrolled";
  if (people === 0) {
    who = "No one yet. People you enroll later start right away.";
  } else if (people !== null) {
    who = `${plural(people, "person", "people")} enrolled`;
  }
  return (
    <ConfirmDialog
      blocked={unsaved || !first || pool?.length === 0}
      confirmLabel="Start sending"
      description={
        unsaved ? (
          <span className="text-warning">
            You have changes that are not saved yet. Save them first, so the
            campaign starts with what you see.
          </span>
        ) : (
          "Each person gets the sequence one email at a time, inside the send window. You can pause it whenever you like."
        )
      }
      onConfirm={onConfirm}
      onOpenChange={onOpenChange}
      open={open}
      title={`Start “${campaign.name}”?`}
    >
      <dl className="grid grid-cols-[88px_1fr] gap-x-4 gap-y-2.5 text-sm">
        <Fact label="Who">{who}</Fact>
        <Fact label="First email">
          {first ? (
            first.name
          ) : (
            <span className="text-warning">The sequence has no email yet.</span>
          )}
        </Fact>
        <Fact label="From">{from}</Fact>
        <Fact label="When">
          {formatWindow(schedule.send_window)}, {zoneName(schedule.timezone)}
          {schedule.start_at ? (
            <span className="text-fg-3 block">
              Not before {formatWhenIn(schedule.start_at, schedule.timezone)}
            </span>
          ) : null}
        </Fact>
      </dl>
    </ConfirmDialog>
  );
};

/**
 * Deleting a campaign, honestly worded: one that never created a message is removed for good;
 * one that did is archived (it stops, its live enrollments stop, its history stays). Which one
 * is asked of the API (does any message point at it?) when the dialog opens.
 */
const DeleteCampaignDialog = ({
  campaign,
  onOpenChange,
  open,
}: {
  campaign: CampaignObject;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const sent = useQuery({
    ...hasMessagesQuery(workspace, campaign.id),
    enabled: open,
  });
  const archives = sent.data ?? campaign.status !== "draft";
  let description = archives
    ? "It has messages, so it is archived: it stops sending, its live enrollments stop and their queued messages are cancelled. Its history and counters stay readable."
    : "It never created a message, so it is removed with its steps and enrollments. This cannot be undone.";
  if (sent.isPending) {
    description = "Checking whether it has messages…";
  }
  return (
    <ConfirmDialog
      confirmLabel={archives ? "Archive campaign" : "Delete campaign"}
      danger
      description={description}
      onConfirm={async () => {
        try {
          const answer: unknown = await workspace.api.campaigns.delete(
            campaign.id
          );
          // `204` (no body) means it was removed; the archived campaign comes back otherwise.
          const archived = typeof answer === "object" && answer !== null;
          toast.success(archived ? "Campaign archived" : "Campaign deleted");
          if (archived) {
            await queryClient.invalidateQueries({
              queryKey: campaignsKey(workspace),
            });
            return;
          }
          await navigate({
            params: { slug: workspace.slug },
            to: "/w/$slug/campaigns",
          });
          queryClient.removeQueries({
            queryKey: campaignKey(workspace, campaign.id),
          });
          await queryClient.invalidateQueries({
            queryKey: campaignsKey(workspace),
          });
        } catch (error) {
          toast.error(describeProblem(error).detail);
        }
      }}
      onOpenChange={onOpenChange}
      open={open}
      title={
        archives ? `Archive ${campaign.name}?` : `Delete ${campaign.name}?`
      }
    />
  );
};

/**
 * The campaign page's actions: start a draft, resume a paused campaign or pause an active one,
 * and a menu to rename it, copy its id, and delete or archive it. A viewer sees only the copy.
 */
export const CampaignActions = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const control = useRunControl(campaign);
  const [dialog, setDialog] = useState<"rename" | "delete" | "start" | null>(
    null
  );
  const writable = canWrite(workspace);
  const key = campaignsKey(workspace);

  return (
    <>
      {writable && control ? (
        <Button
          onClick={() => {
            if (campaign.status === "draft") {
              setDialog("start");
              return;
            }
            void control.handleRun();
          }}
          variant={control.starts ? "primary" : "secondary"}
        >
          <HugeiconsIcon icon={control.starts ? PlayIcon : PauseIcon} />
          {control.label}
        </Button>
      ) : null}
      <RowMenu label="Campaign actions">
        {writable && campaign.status !== "archived" ? (
          <DropdownMenuItem onClick={() => setDialog("rename")}>
            Rename
          </DropdownMenuItem>
        ) : null}
        <CopyIdItem id={campaign.id} noun="campaign" />
        {writable && campaign.status !== "archived" ? (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className="text-error-fg"
              onClick={() => setDialog("delete")}
            >
              {campaign.status === "draft" ? "Delete" : "Archive"}
            </DropdownMenuItem>
          </>
        ) : null}
      </RowMenu>
      <CreateDialog
        fields={[
          {
            initial: campaign.name,
            label: "Name",
            name: "name",
            required: true,
          },
        ]}
        key={campaign.name}
        onOpenChange={(open) => setDialog(open ? "rename" : null)}
        onSubmit={async (values) => {
          const updated = await workspace.api.campaigns.update(campaign.id, {
            name: values.name,
          });
          queryClient.setQueryData(
            campaignKey(workspace, campaign.id),
            updated
          );
          toast.success("Campaign renamed");
          setDialog(null);
          await queryClient.invalidateQueries({ queryKey: [...key, "list"] });
        }}
        open={dialog === "rename"}
        submitLabel="Rename"
        title="Rename campaign"
      />
      {control ? (
        <StartDialog
          campaign={campaign}
          onConfirm={control.handleRun}
          onOpenChange={(open) => setDialog(open ? "start" : null)}
          open={dialog === "start"}
        />
      ) : null}
      <DeleteCampaignDialog
        campaign={campaign}
        onOpenChange={(open) => setDialog(open ? "delete" : null)}
        open={dialog === "delete"}
      />
    </>
  );
};
