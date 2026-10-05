import type { MessageObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import type { DetailRow } from "@/components/details";
import { PageBody, PageHeader } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Card, CardContent } from "@/components/ui/card";
import { DeliveryHistory } from "@/features/messages/delivery-history";
import { MessageActions } from "@/features/messages/message-actions";
import { AttemptsCard, HoldsCard } from "@/features/messages/message-sections";
import { messageQuery } from "@/features/messages/queries";
import {
  CampaignLink,
  MailboxName,
  PersonLink,
} from "@/features/messages/related";
import {
  formatAddress,
  formatSubject,
  formatTimestamp,
  humanize,
} from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** Rows for the addresses that are present (Cc, Bcc and Reply-To often are not). */
const addressRows = (message: MessageObject): DetailRow[] => [
  { label: "From", value: formatAddress(message.from) },
  { label: "To", value: message.to.join(", ") },
  ...(message.cc.length > 0
    ? [{ label: "Cc", value: message.cc.join(", ") }]
    : []),
  ...(message.bcc.length > 0
    ? [{ label: "Bcc", value: message.bcc.join(", ") }]
    : []),
  ...(message.reply_to ? [{ label: "Reply-To", value: message.reply_to }] : []),
  { label: "Kind", value: humanize(message.kind) },
];

/** When it was created, is due, expires and was sent. */
const timeRows = (message: MessageObject): DetailRow[] => [
  { label: "Created", value: formatTimestamp(message.created_at) },
  { label: "Send at", value: formatTimestamp(message.send_at) },
  ...(message.expires_at
    ? [{ label: "Expires", value: formatTimestamp(message.expires_at) }]
    : []),
  {
    label: "Sent",
    value: message.sent_at ? formatTimestamp(message.sent_at) : "Not yet",
  },
  { label: "Attempts", value: String(message.attempts_count) },
];

/** What the message tracks, frozen when it was created. */
const trackingRows = (message: MessageObject): DetailRow[] => [
  { label: "Opens", value: message.tracking.opens ? "Tracked" : "Off" },
  { label: "Clicks", value: message.tracking.clicks ? "Tracked" : "Off" },
  ...(message.tracking.hostname
    ? [{ label: "Host", value: message.tracking.hostname }]
    : []),
  ...(message.snippets_fallback
    ? [
        {
          label: "Snippets",
          value: `Template defaults used (${humanize(message.snippets_fallback)})`,
        },
      ]
    : []),
];

/** Links to what the message belongs to: its conversation, campaign, person and mailbox. */
const RelatedSection = ({ message }: { message: MessageObject }) => {
  const workspace = useWorkspace();
  return (
    <DetailSection
      rows={[
        ...(message.thread_id
          ? [
              {
                label: "Conversation",
                value: (
                  <Link
                    params={{
                      slug: workspace.slug,
                      threadId: message.thread_id,
                    }}
                    to="/w/$slug/inbox/$threadId"
                  >
                    Open in the inbox
                  </Link>
                ),
              },
            ]
          : []),
        ...(message.campaign_id
          ? [
              {
                label: "Campaign",
                value: <CampaignLink id={message.campaign_id} />,
              },
            ]
          : []),
        ...(message.person_id
          ? [{ label: "Person", value: <PersonLink id={message.person_id} /> }]
          : []),
        {
          label: "Mailbox",
          value: (
            <MailboxName
              id={message.connection_id}
              identityId={message.sender_identity_id}
            />
          ),
        },
      ]}
      title="Related"
    />
  );
};

/** Why an uncertain message waits for a person, above everything else on its page. */
const UncertainNotice = () => (
  <Alert variant="warning">
    <AlertTitle>Waiting for your decision</AlertTitle>
    <AlertDescription>
      The submission ended without a readable answer: the provider may have
      taken it, so it is never sent again on its own. Resolve it as sent or
      failed once you know.
    </AlertDescription>
  </Alert>
);

/**
 * One message: its state and why, its holds, its attempts and the delivery events reported for
 * it, with what it belongs to beside them; the header offers what can still be done to it.
 */
const MessagePage = () => {
  const workspace = useWorkspace();
  const { messageId } = Route.useParams();
  const { data: message } = useSuspenseQuery(
    messageQuery(workspace, messageId)
  );
  return (
    <PageBody>
      <PageHeader
        actions={<MessageActions message={message} />}
        compact
        back={{
          label: "Messages",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/messages",
          },
        }}
        subtitle={<StatusBadge kind="message" value={message.state} />}
        title={formatSubject(message.subject)}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-3">
          {message.state === "uncertain" ? <UncertainNotice /> : null}
          {message.status_detail ? (
            <Card>
              <CardContent className="text-fg-2 text-sm">
                {message.status_detail}
              </CardContent>
            </Card>
          ) : null}
          {message.holds.length > 0 ? (
            <HoldsCard holds={message.holds} />
          ) : null}
          <AttemptsCard message={message} />
          <DeliveryHistory messageId={messageId} />
        </div>
        <DetailsAside>
          <DetailSection rows={addressRows(message)} title="Message" />
          <DetailSection rows={timeRows(message)} title="Delivery" />
          <DetailSection rows={trackingRows(message)} title="Tracking" />
          <RelatedSection message={message} />
          <DetailSection
            rows={[
              { label: "ID", value: <Copyable mono value={message.id} /> },
              {
                label: "Message-ID",
                value: <Copyable mono value={message.internet_message_id} />,
              },
              ...(message.in_reply_to
                ? [
                    {
                      label: "In-Reply-To",
                      value: <Copyable mono value={message.in_reply_to} />,
                    },
                  ]
                : []),
            ]}
            title="Identifiers"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/messages/$messageId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      messageQuery(context.workspace, params.messageId)
    ),
  head: () => ({ meta: [{ title: "Message · Norbelys" }] }),
  component: MessagePage,
});
