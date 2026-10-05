import type { ThreadObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import type { DetailRow } from "@/components/details";
import { PageBody, PageHeader } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { threadQuery } from "@/features/inbox/queries";
import { ReplyBox } from "@/features/inbox/reply-box";
import { ThreadActions } from "@/features/inbox/thread-actions";
import { ThreadTimeline } from "@/features/inbox/thread-timeline";
import {
  CampaignLink,
  MailboxName,
  PersonLink,
} from "@/features/messages/related";
import { formatSubject, formatTimestamp } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** Where the conversation stands: its status (and until when it sleeps), unread, its activity. */
const stateRows = (thread: ThreadObject): DetailRow[] => [
  {
    label: "Status",
    value: <StatusBadge kind="thread" value={thread.status} />,
  },
  ...(thread.snoozed_until
    ? [{ label: "Until", value: formatTimestamp(thread.snoozed_until) }]
    : []),
  { label: "Unread", value: thread.unread ? "Yes" : "No" },
  { label: "Last activity", value: formatTimestamp(thread.last_activity_at) },
  { label: "Started", value: formatTimestamp(thread.created_at) },
];

/** The mailbox and identity it is held through, its campaign and person, and all its messages. */
const RelatedSection = ({ thread }: { thread: ThreadObject }) => {
  const workspace = useWorkspace();
  return (
    <DetailSection
      rows={[
        {
          label: "Mailbox",
          value: (
            <MailboxName
              id={thread.connection_id}
              identityId={thread.sender_identity_id}
            />
          ),
        },
        ...(thread.campaign_id
          ? [
              {
                label: "Campaign",
                value: <CampaignLink id={thread.campaign_id} />,
              },
            ]
          : []),
        ...(thread.person_id
          ? [{ label: "Person", value: <PersonLink id={thread.person_id} /> }]
          : []),
        {
          label: "History",
          value: (
            <span className="flex flex-col">
              <Link
                params={{ slug: workspace.slug }}
                search={{ thread_id: thread.id }}
                to="/w/$slug/messages"
              >
                Messages sent
              </Link>
              <Link
                params={{ slug: workspace.slug }}
                search={{ thread_id: thread.id }}
                to="/w/$slug/inbox/received"
              >
                Mail received
              </Link>
            </span>
          ),
        },
      ]}
      title="Related"
    />
  );
};

/**
 * One conversation: our messages and their answers as a timeline, a reply under it once someone
 * wrote back, and the header's read, snooze and archive actions; beside it, its state, the people
 * taking part and what it belongs to.
 */
const ThreadPage = () => {
  const workspace = useWorkspace();
  const { threadId } = Route.useParams();
  const { data: thread } = useSuspenseQuery(threadQuery(workspace, threadId));
  return (
    <PageBody>
      <PageHeader
        actions={<ThreadActions thread={thread} />}
        compact
        back={{
          label: "Inbox",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/inbox",
          },
        }}
        subtitle={
          <>
            <StatusBadge kind="thread" value={thread.status} />
            {thread.unread ? (
              <Badge className="ml-1" tone="info">
                Unread
              </Badge>
            ) : null}
          </>
        }
        title={formatSubject(thread.subject)}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-3">
          <ThreadTimeline thread={thread} />
          <ReplyBox thread={thread} />
        </div>
        <DetailsAside>
          <DetailSection rows={stateRows(thread)} title="Conversation" />
          <DetailSection
            rows={thread.participants.map((address, index) => ({
              label: index === 0 ? "Our address" : "With",
              value: address,
            }))}
            title="Participants"
          />
          <RelatedSection thread={thread} />
          <DetailSection
            rows={[{ label: "ID", value: <Copyable mono value={thread.id} /> }]}
            title="Identifiers"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/inbox/$threadId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      threadQuery(context.workspace, params.threadId)
    ),
  head: () => ({ meta: [{ title: "Conversation · Norbelys" }] }),
  component: ThreadPage,
});
