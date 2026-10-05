import type { InboundMessageObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import type { DetailRow } from "@/components/details";
import { PageBody, PageHeader } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { ClassificationCard } from "@/features/inbox/classification-card";
import { ReviewCard } from "@/features/inbox/inbound-review";
import { inboundQuery } from "@/features/inbox/queries";
import { MailboxName, PersonLink } from "@/features/messages/related";
import {
  formatAddress,
  formatCount,
  formatSubject,
  formatTimestamp,
} from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** Who wrote, as the message gave it. */
const senderText = (message: InboundMessageObject): string =>
  message.from ? formatAddress(message.from) : "Unknown sender";

/** The start of the text the inbox kept, and whether the message was larger than that. */
const TextCard = ({ message }: { message: InboundMessageObject }) => (
  <Card>
    <CardHeader className="flex-col items-start gap-0.5">
      <CardTitle className="break-all">{senderText(message)}</CardTitle>
      <CardDescription className="text-xs">
        Received {formatTimestamp(message.received_at)}
        {message.truncated
          ? " · larger than what the inbox keeps: this is its start"
          : ""}
      </CardDescription>
    </CardHeader>
    <CardContent className="border-line border-t pt-4">
      {message.text ? (
        <p className="text-fg-2 text-sm break-words whitespace-pre-wrap">
          {message.text}
        </p>
      ) : (
        <p className="text-fg-3 text-sm">
          The message has no text of its own to show.
        </p>
      )}
    </CardContent>
  </Card>
);

/** The message as it arrived: sender, subject, size and the mailbox that read it. */
const messageRows = (message: InboundMessageObject): DetailRow[] => [
  { label: "From", value: senderText(message) },
  { label: "Subject", value: formatSubject(message.subject) },
  { label: "Received", value: formatTimestamp(message.received_at) },
  ...(message.size_bytes === null || message.size_bytes === undefined
    ? []
    : [{ label: "Size", value: `${formatCount(message.size_bytes)} bytes` }]),
  { label: "Mailbox", value: <MailboxName id={message.connection_id} /> },
];

/** The conversation, the message it answers or reports on, and the person who wrote. */
const RelatedSection = ({ message }: { message: InboundMessageObject }) => {
  const workspace = useWorkspace();
  const rows: DetailRow[] = [
    ...(message.thread_id
      ? [
          {
            label: "Conversation",
            value: (
              <Link
                params={{ slug: workspace.slug, threadId: message.thread_id }}
                to="/w/$slug/inbox/$threadId"
              >
                Open the conversation
              </Link>
            ),
          },
        ]
      : []),
    ...(message.message_id
      ? [
          {
            label: "Answers",
            value: (
              <Link
                params={{ messageId: message.message_id, slug: workspace.slug }}
                to="/w/$slug/messages/$messageId"
              >
                The message we sent
              </Link>
            ),
          },
        ]
      : []),
    ...(message.person_id
      ? [{ label: "Person", value: <PersonLink id={message.person_id} /> }]
      : []),
  ];
  if (rows.length === 0) {
    return null;
  }
  return <DetailSection rows={rows} title="Related" />;
};

/** Its own identifiers and the headers that tie it to our messages. */
const identifierRows = (message: InboundMessageObject): DetailRow[] => [
  { label: "ID", value: <Copyable mono value={message.id} /> },
  ...(message.internet_message_id
    ? [
        {
          label: "Message-ID",
          value: <Copyable mono value={message.internet_message_id} />,
        },
      ]
    : []),
  ...(message.in_reply_to
    ? [
        {
          label: "In-Reply-To",
          value: <Copyable mono value={message.in_reply_to} />,
        },
      ]
    : []),
  ...(message.references.length > 0
    ? [
        {
          label: "References",
          value: (
            <span className="flex flex-col gap-1">
              {message.references.map((reference) => (
                <Copyable key={reference} mono value={reference} />
              ))}
            </span>
          ),
        },
      ]
    : []),
];

/**
 * One message the inbox read: the review it asks for, its text, how it was classified and on what
 * evidence (with the correction), and beside it what it belongs to and its headers.
 */
const ReceivedMessagePage = () => {
  const workspace = useWorkspace();
  const { inboundId } = Route.useParams();
  const { data: message } = useSuspenseQuery(
    inboundQuery(workspace, inboundId)
  );
  return (
    <PageBody>
      <PageHeader
        compact
        back={{
          label: "Received mail",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/inbox/received",
          },
        }}
        subtitle={
          <StatusBadge kind="classification" value={message.classification} />
        }
        title={formatSubject(message.subject)}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-3">
          <ReviewCard message={message} />
          <TextCard message={message} />
          <ClassificationCard key={message.version} message={message} />
        </div>
        <DetailsAside>
          <DetailSection rows={messageRows(message)} title="Message" />
          <RelatedSection message={message} />
          <DetailSection rows={identifierRows(message)} title="Identifiers" />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/inbox/received/$inboundId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      inboundQuery(context.workspace, params.inboundId)
    ),
  head: () => ({ meta: [{ title: "Received message · Norbelys" }] }),
  component: ReceivedMessagePage,
});
