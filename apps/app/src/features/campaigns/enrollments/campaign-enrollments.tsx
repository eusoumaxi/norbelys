import { UserAdd01Icon, UserMultiple02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type {
  CampaignObject,
  EnrollmentObject,
  EnrollmentStatus,
} from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { ContactCell, Dash, ListTable } from "@/components/data-table";
import type { Column } from "@/components/data-table";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Segmented } from "@/components/ui/segmented";
import type { SegmentedOption } from "@/components/ui/segmented";
import { EnrollDialog } from "@/features/campaigns/enrollments/enroll-dialog";
import { identityLabel } from "@/features/campaigns/pool";
import {
  campaignEditable,
  campaignsKey,
  enrollmentListQuery,
  enrollmentsKey,
  sendersQuery,
  useCampaign,
} from "@/features/campaigns/queries";
import type { SenderIdentity } from "@/features/campaigns/queries";
import { useAction } from "@/lib/actions";
import {
  formatDateTime,
  formatRelative,
  formatWhenIn,
  shortId,
  zoneName,
} from "@/lib/format";
import { statusLabel } from "@/lib/status";
import { useWorkspace } from "@/lib/workspace";

type Filter = "all" | EnrollmentStatus;

const FILTERS: SegmentedOption<Filter>[] = [
  { label: "All", value: "all" },
  { label: "Active", value: "active" },
  { label: "Paused", value: "paused" },
  { label: "Replied", value: "replied" },
  { label: "Completed", value: "completed" },
  { label: "Stopped", value: "stopped" },
  { label: "Failed", value: "failed" },
];

/** Whether an enrollment can still be stopped: it has not ended. */
const live = (enrollment: EnrollmentObject) =>
  enrollment.status === "active" || enrollment.status === "paused";

/** The sender of an id (`sid_…`), as its address, or the short id while senders load. */
const senderName = (senders: SenderIdentity[] | undefined, id: string) => {
  const sender = senders?.find((s) => s.identity.id === id);
  return sender ? identityLabel(sender) : shortId(id);
};

/** The sender a conversation keeps, or the one its next step waits for. */
const SenderCell = ({
  enrollment,
  senders,
}: {
  enrollment: EnrollmentObject;
  senders: SenderIdentity[] | undefined;
}) => {
  if (enrollment.waiting_for) {
    return (
      <span className="text-warning block max-w-[220px] truncate">
        Waiting for {senderName(senders, enrollment.waiting_for)}
      </span>
    );
  }
  if (enrollment.sender_identity_id) {
    return (
      <span className="text-fg-2 block max-w-[220px] truncate">
        {senderName(senders, enrollment.sender_identity_id)}
      </span>
    );
  }
  return <Dash />;
};

/**
 * When the person's next email is due, as the campaign's calendar reads it (its send window is in
 * that zone), with the person's own clock on hover; or that it is on its way.
 */
const NextCell = ({
  enrollment,
  zone,
}: {
  enrollment: EnrollmentObject;
  zone: string;
}) => {
  if (enrollment.next_run_at && live(enrollment)) {
    return (
      <time
        className="flex flex-col"
        dateTime={enrollment.next_run_at}
        title={`${formatDateTime(enrollment.next_run_at)} your time`}
      >
        <span className="first-letter:uppercase">
          {formatWhenIn(enrollment.next_run_at, zone)}
        </span>
        <span className="text-fg-3 text-xs">{zoneName(zone)}</span>
      </time>
    );
  }
  if (enrollment.message_id && live(enrollment)) {
    return <span className="text-fg-2">On its way</span>;
  }
  return <Dash />;
};

const columns = ({
  campaign,
  onOpenMessage,
  onStop,
  senders,
  writable,
}: {
  campaign: CampaignObject;
  onOpenMessage: (id: string) => void;
  onStop: (enrollment: EnrollmentObject) => void;
  senders: SenderIdentity[] | undefined;
  writable: boolean;
}): Column<EnrollmentObject>[] => [
  {
    render: (e) => <ContactCell email={e.person.email} name={e.person.name} />,
    header: "Person",
    id: "person",
  },
  {
    render: (e) => (
      <span className="flex flex-col items-start gap-1">
        <StatusBadge kind="enrollment" value={e.status} />
        {e.status_detail ? (
          <span className="text-fg-3 max-w-[240px] text-xs">
            {e.status_detail}
          </span>
        ) : null}
      </span>
    ),
    header: "Status",
    id: "status",
  },
  {
    render: (e) => {
      const step = campaign.steps[e.position - 1];
      return (
        <span className="flex flex-col">
          <span className="text-fg max-w-[180px] truncate">
            {step ? step.name : `Email ${e.position}`}
          </span>
          <span className="text-fg-3 text-xs tabular-nums">
            {e.position} of {campaign.steps.length}
          </span>
        </span>
      );
    },
    header: "Email",
    id: "step",
  },
  {
    render: (e) => (
      <NextCell enrollment={e} zone={campaign.schedule.timezone} />
    ),
    header: "Next email",
    id: "next",
  },
  {
    render: (e) => <SenderCell enrollment={e} senders={senders} />,
    header: "Sender",
    id: "sender",
  },
  {
    render: (e) => formatRelative(e.created_at),
    header: "Enrolled",
    id: "created",
  },
  {
    render: (e) => (
      <RowMenu>
        {e.message_id ? (
          <DropdownMenuItem
            onClick={() => {
              if (e.message_id) {
                onOpenMessage(e.message_id);
              }
            }}
          >
            Open current message
          </DropdownMenuItem>
        ) : null}
        <CopyIdItem id={e.id} noun="enrollment" />
        {writable && live(e) ? (
          <DropdownMenuItem className="text-error-fg" onClick={() => onStop(e)}>
            Stop
          </DropdownMenuItem>
        ) : null}
      </RowMenu>
    ),
    className: "w-[62px]",
    header: "",
    id: "menu",
  },
];

/**
 * The people enrolled in the campaign, filtered by status: where each one is in the sequence,
 * when its next step is due, which sender its conversation keeps (or waits for), and why it
 * ended. People are enrolled from here, and a live enrollment can be stopped.
 */
export const CampaignEnrollments = () => {
  const campaign = useCampaign();
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const action = useAction();
  const senders = useQuery(sendersQuery(workspace));
  const [filter, setFilter] = useState<Filter>("all");
  const [enrolling, setEnrolling] = useState(false);
  const [stopping, setStopping] = useState<EnrollmentObject | null>(null);
  const writable = campaignEditable(workspace, campaign);
  const status = filter === "all" ? undefined : filter;
  const enrollButton = writable ? (
    <Button onClick={() => setEnrolling(true)} size="s" variant="primary">
      <HugeiconsIcon icon={UserAdd01Icon} />
      Enroll people
    </Button>
  ) : null;

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Segmented
          className="overflow-x-auto"
          label="Status"
          onChange={setFilter}
          options={FILTERS}
          value={filter}
        />
        {enrollButton}
      </div>
      <ListTable<EnrollmentObject>
        columns={columns({
          campaign,
          onOpenMessage: (messageId) => {
            void navigate({
              params: { messageId, slug: workspace.slug },
              to: "/w/$slug/messages/$messageId",
            });
          },
          onStop: setStopping,
          senders: senders.data,
          writable,
        })}
        empty={
          status
            ? {
                description: `No enrollment of this campaign is ${statusLabel("enrollment", status).toLowerCase()}.`,
                icon: UserMultiple02Icon,
                title: "None in this state",
              }
            : {
                action: enrollButton ?? undefined,
                description:
                  "Add people by address, from a group or a segment. Each one starts with the first email.",
                icon: UserMultiple02Icon,
                illustration: "people",
                title: "No one is enrolled yet",
              }
        }
        query={enrollmentListQuery(workspace, {
          campaign_id: campaign.id,
          status,
        })}
        rowKey={(e) => e.id}
      />
      <EnrollDialog
        campaignId={campaign.id}
        onOpenChange={setEnrolling}
        open={enrolling}
      />
      <ConfirmDialog
        confirmLabel="Stop enrollment"
        danger
        description={
          stopping
            ? `${stopping.person.email} gets no further step of this campaign, and the message still queued for them is cancelled. This cannot be undone; enrolling them again starts over.`
            : ""
        }
        onConfirm={() =>
          stopping
            ? action(
                "Enrollment stopped",
                () => workspace.api.enrollments.stop(stopping.id),
                () =>
                  Promise.all([
                    queryClient.invalidateQueries({
                      queryKey: enrollmentsKey(workspace),
                    }),
                    queryClient.invalidateQueries({
                      queryKey: campaignsKey(workspace),
                    }),
                  ])
              )
            : undefined
        }
        onOpenChange={(open) => {
          if (!open) {
            setStopping(null);
          }
        }}
        open={stopping !== null}
        title="Stop this enrollment?"
      />
    </div>
  );
};
