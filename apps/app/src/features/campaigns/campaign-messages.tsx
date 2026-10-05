import { Mail01Icon } from "@hugeicons/core-free-icons";
import type { MessageObject } from "@norbelys/sdk";
import { useNavigate } from "@tanstack/react-router";

import { Dash, ListTable } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { RelativeTime } from "@/components/time";
import { useCampaign } from "@/features/campaigns/queries";
import { messageListQuery } from "@/features/messages/queries";
import { formatSubject } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/**
 * The messages the campaign created, newest first: subject, recipient, sender, state and step,
 * each opening the message with its delivery history.
 */
export const CampaignMessages = () => {
  const campaign = useCampaign();
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const stepName = (id: string | null | undefined) => {
    const step = campaign.steps.find((s) => s.id === id);
    return step ? `${step.position}. ${step.name}` : null;
  };
  return (
    <ListTable<MessageObject>
      columns={[
        {
          render: (m) => (
            <span className="text-fg block max-w-[320px] truncate font-semibold">
              {formatSubject(m.subject)}
            </span>
          ),
          header: "Subject",
          id: "subject",
        },
        {
          render: (m) => (
            <span className="text-fg-2 block max-w-[220px] truncate">
              {m.to.join(", ")}
            </span>
          ),
          header: "To",
          id: "to",
        },
        {
          render: (m) => (
            <span className="text-fg-2 block max-w-[200px] truncate">
              {m.from.email}
            </span>
          ),
          header: "From",
          id: "from",
        },
        {
          render: (m) => <StatusBadge kind="message" value={m.state} />,
          header: "Status",
          id: "state",
        },
        {
          render: (m) => (
            <span className="text-fg-2 block max-w-[180px] truncate">
              {stepName(m.step_id) ?? <Dash />}
            </span>
          ),
          header: "Email",
          id: "step",
        },
        {
          render: (m) => <RelativeTime value={m.created_at} />,
          header: "Created",
          id: "created",
        },
      ]}
      empty={{
        description:
          campaign.status === "draft"
            ? "Each email appears here when it is written for a person: once the campaign is started, inside its send window."
            : "Each email appears here when it is written for a person, inside the send window. The first ones are on their way.",
        icon: Mail01Icon,
        illustration: "first-send",
        title: "No emails yet",
      }}
      onRowClick={(m) => {
        void navigate({
          params: { messageId: m.id, slug: workspace.slug },
          to: "/w/$slug/messages/$messageId",
        });
      }}
      query={messageListQuery(workspace, { campaign_id: campaign.id })}
      rowKey={(m) => m.id}
    />
  );
};
