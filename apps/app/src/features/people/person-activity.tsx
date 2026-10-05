import {
  InboxIcon,
  Mail01Icon,
  Megaphone01Icon,
} from "@hugeicons/core-free-icons";
import type {
  EnrollmentObject,
  MessageObject,
  ThreadObject,
} from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { parseAsStringLiteral, useQueryState } from "nuqs";

import { Dash, ListTable, NameCell } from "@/components/data-table";
import { Section } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsList, TabsPanel, TabsTab } from "@/components/ui/tabs";
import {
  campaignOptionsQuery,
  enrollmentListQuery,
} from "@/features/campaigns/queries";
import { LastMessage } from "@/features/inbox/classification";
import { threadListQuery } from "@/features/inbox/queries";
import { messageListQuery } from "@/features/messages/queries";
import { formatRelative, formatSubject, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

const TABS = ["messages", "enrollments", "conversations"] as const;

/** The messages sent, or queued, to this person; a row opens the message. */
const Messages = ({ personId }: { personId: string }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <ListTable<MessageObject>
      columns={[
        {
          render: (m) => (
            <NameCell icon={Mail01Icon}>{formatSubject(m.subject)}</NameCell>
          ),
          header: "Subject",
          id: "subject",
        },
        {
          render: (m) => <StatusBadge kind="message" value={m.state} />,
          header: "State",
          id: "state",
        },
        {
          render: (m) => <span className="text-fg-2">{m.from.email}</span>,
          header: "From",
          id: "from",
        },
        {
          render: (m) => (m.sent_at ? formatRelative(m.sent_at) : <Dash />),
          header: "Sent",
          id: "sent",
        },
        {
          render: (m) => formatRelative(m.created_at),
          header: "Created",
          id: "created",
        },
      ]}
      empty={{
        description: "Nothing has been sent to this person yet.",
        icon: Mail01Icon,
        title: "No messages",
      }}
      onRowClick={(m) => {
        void navigate({
          params: { messageId: m.id, slug: workspace.slug },
          to: "/w/$slug/messages/$messageId",
        });
      }}
      query={messageListQuery(workspace, { person_id: personId })}
      rowKey={(m) => m.id}
    />
  );
};

/** The campaigns this person is enrolled in; a row opens the campaign. */
const Enrollments = ({ personId }: { personId: string }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const campaigns = useQuery(campaignOptionsQuery(workspace));
  const names = new Map(
    (campaigns.data?.data ?? []).map((campaign) => [campaign.id, campaign.name])
  );
  return (
    <ListTable<EnrollmentObject>
      columns={[
        {
          render: (e) => (
            <NameCell icon={Megaphone01Icon}>
              {names.get(e.campaign_id) ?? shortId(e.campaign_id)}
            </NameCell>
          ),
          header: "Campaign",
          id: "campaign",
        },
        {
          render: (e) => <StatusBadge kind="enrollment" value={e.status} />,
          header: "Status",
          id: "status",
        },
        {
          render: (e) => `Step ${e.position}`,
          header: "Position",
          id: "position",
        },
        {
          render: (e) =>
            e.next_run_at ? formatRelative(e.next_run_at) : <Dash />,
          header: "Next step",
          id: "next",
        },
        {
          render: (e) => formatRelative(e.created_at),
          header: "Enrolled",
          id: "created",
        },
      ]}
      empty={{
        description: "This person has not been enrolled in a campaign.",
        icon: Megaphone01Icon,
        title: "No enrollments",
      }}
      onRowClick={(e) => {
        void navigate({
          params: { campaignId: e.campaign_id, slug: workspace.slug },
          to: "/w/$slug/campaigns/$campaignId",
        });
      }}
      query={enrollmentListQuery(workspace, { person_id: personId })}
      rowKey={(e) => e.id}
    />
  );
};

/** The conversations with this person: our messages and their replies, by thread. */
const Conversations = ({ personId }: { personId: string }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <ListTable<ThreadObject>
      columns={[
        {
          render: (t) => (
            <span className="flex min-w-0 items-center gap-2">
              <NameCell icon={InboxIcon}>{formatSubject(t.subject)}</NameCell>
              {t.unread ? <Badge tone="info">Unread</Badge> : null}
            </span>
          ),
          header: "Subject",
          id: "subject",
        },
        {
          render: (t) => <StatusBadge kind="thread" value={t.status} />,
          header: "Status",
          id: "status",
        },
        {
          render: (t) => <LastMessage thread={t} />,
          header: "Last message",
          id: "last",
        },
        {
          render: (t) => formatRelative(t.last_activity_at),
          header: "Last activity",
          id: "activity",
        },
      ]}
      empty={{
        description: "No thread with this person yet.",
        icon: InboxIcon,
        title: "No conversations",
      }}
      onRowClick={(t) => {
        void navigate({
          params: { slug: workspace.slug, threadId: t.id },
          to: "/w/$slug/inbox/$threadId",
        });
      }}
      query={threadListQuery(workspace, { person_id: personId })}
      rowKey={(t) => t.id}
    />
  );
};

/**
 * A person's activity in three tabs: the messages sent to them, their campaign enrollments and
 * their conversations, each read with the list's own `person_id` filter and opening its own page.
 * The tab is the `activity` search parameter, so coming back from a message lands on it again.
 */
export const PersonActivity = ({ personId }: { personId: string }) => {
  const [tab, setTab] = useQueryState(
    "activity",
    parseAsStringLiteral(TABS).withDefault("messages")
  );
  return (
    <Section title="Activity">
      <Tabs
        onValueChange={(value: (typeof TABS)[number]) => {
          void setTab(value === "messages" ? null : value);
        }}
        value={tab}
      >
        <TabsList>
          <TabsTab value="messages">Messages</TabsTab>
          <TabsTab value="enrollments">Enrollments</TabsTab>
          <TabsTab value="conversations">Conversations</TabsTab>
        </TabsList>
        <TabsPanel value="messages">
          <Messages personId={personId} />
        </TabsPanel>
        <TabsPanel value="enrollments">
          <Enrollments personId={personId} />
        </TabsPanel>
        <TabsPanel value="conversations">
          <Conversations personId={personId} />
        </TabsPanel>
      </Tabs>
    </Section>
  );
};
