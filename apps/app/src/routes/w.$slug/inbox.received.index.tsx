import { MailReceive01Icon } from "@hugeicons/core-free-icons";
import type { InboundMessageObject } from "@norbelys/sdk";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { z } from "zod";

import { ContactCell, Dash, ListTable } from "@/components/data-table";
import { PageBody } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { RelativeTime } from "@/components/time";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import {
  classificationOptions,
  ReviewBadge,
  SOURCES,
} from "@/features/inbox/classification";
import { InboxHeader } from "@/features/inbox/inbox-header";
import { CLASSIFICATIONS, inboundListQuery } from "@/features/inbox/queries";
import type { InboundFilters } from "@/features/inbox/queries";
import {
  ALL,
  FilterChip,
  MailboxFilter,
} from "@/features/messages/message-filters";
import { formatSubject } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

type ReviewChoice = "all" | "pending";

const REVIEW_OPTIONS: { label: string; value: ReviewChoice }[] = [
  { label: "All", value: "all" },
  { label: "Needs review", value: "pending" },
];

const CLASSIFICATION_OPTIONS = classificationOptions({
  label: "All classifications",
  value: ALL,
});

/** The filter row: review state, classification, mailbox, and a thread a link narrowed to. */
const Filters = ({
  filters,
  onChange,
}: {
  filters: InboundFilters;
  onChange: (patch: InboundFilters) => void;
}) => (
  <div className="flex flex-wrap items-center gap-2">
    <Segmented<ReviewChoice>
      label="Review"
      onChange={(next) =>
        onChange({ review: next === "pending" ? "pending" : undefined })
      }
      options={REVIEW_OPTIONS}
      value={filters.review ?? "all"}
    />
    <Select
      className="w-52"
      label="Classification"
      onChange={(next) =>
        onChange({
          classification: CLASSIFICATIONS.find((value) => value === next),
        })
      }
      options={CLASSIFICATION_OPTIONS}
      value={filters.classification ?? ALL}
    />
    <MailboxFilter
      onChange={(connection_id) => onChange({ connection_id })}
      value={filters.connection_id}
    />
    {filters.thread_id ? (
      <FilterChip
        label="Thread"
        onClear={() => onChange({ thread_id: undefined })}
        value={filters.thread_id}
      />
    ) : null}
    {Object.values(filters).some(Boolean) ? (
      <Button
        onClick={() =>
          onChange({
            classification: undefined,
            connection_id: undefined,
            review: undefined,
            thread_id: undefined,
          })
        }
        variant="tertiary"
      >
        Clear filters
      </Button>
    ) : null}
  </div>
);

/**
 * Every message the inbox read, newest first: replies, auto-replies, bounces, complaints and
 * notices, with how each was classified and by whom, and the ones waiting for a person's review.
 */
const ReceivedMail = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const routeNavigate = Route.useNavigate();
  const filters: InboundFilters = Route.useSearch();
  const filtered = Object.values(filters).some(Boolean);
  return (
    <PageBody>
      <InboxHeader />
      <div className="flex flex-col gap-2">
        <Filters
          filters={filters}
          onChange={(patch) => {
            void routeNavigate({
              replace: true,
              search: (previous) => ({ ...previous, ...patch }),
            });
          }}
        />
        <ListTable<InboundMessageObject>
          columns={[
            {
              render: (message) =>
                message.from ? (
                  <ContactCell
                    className="max-w-[240px]"
                    email={message.from.email}
                    name={message.from.name}
                  />
                ) : (
                  <Dash />
                ),
              header: "From",
              id: "from",
            },
            {
              render: (message) => (
                <span className="text-fg-2 block max-w-[320px] truncate">
                  {formatSubject(message.subject)}
                </span>
              ),
              header: "Subject",
              id: "subject",
            },
            {
              render: (message) => (
                <span className="flex items-center gap-2">
                  <StatusBadge
                    kind="classification"
                    value={message.classification}
                  />
                  <span className="text-fg-3 text-xs">
                    {SOURCES[message.classification_source]}
                  </span>
                </span>
              ),
              header: "Classification",
              id: "classification",
            },
            {
              render: (message) =>
                message.sentiment ? (
                  <StatusBadge
                    dot={false}
                    kind="sentiment"
                    value={message.sentiment}
                  />
                ) : (
                  <Dash />
                ),
              header: "Sentiment",
              id: "sentiment",
            },
            {
              render: (message) =>
                message.review.requested_at ? (
                  <ReviewBadge review={message.review} />
                ) : (
                  <Dash />
                ),
              header: "Review",
              id: "review",
            },
            {
              render: (message) => <RelativeTime value={message.received_at} />,
              header: "Received",
              id: "received",
            },
          ]}
          empty={
            filtered
              ? {
                  description: "No received message matches these filters.",
                  icon: MailReceive01Icon,
                  title: "No results",
                }
              : {
                  description:
                    "What your mailboxes receive is read here and classified: human replies, auto-replies, bounces, complaints and notices.",
                  icon: MailReceive01Icon,
                  title: "Nothing received yet",
                }
          }
          onRowClick={(message) => {
            void navigate({
              params: { inboundId: message.id, slug: workspace.slug },
              to: "/w/$slug/inbox/received/$inboundId",
            });
          }}
          query={inboundListQuery(workspace, filters)}
          rowKey={(message) => message.id}
        />
      </div>
    </PageBody>
  );
};

// The filters as the address keeps them, named as the list filters they feed.
const search = z.object({
  classification: z.enum(CLASSIFICATIONS).optional(),
  connection_id: z.string().optional(),
  review: z.literal("pending").optional(),
  thread_id: z.string().optional(),
});

export const Route = createFileRoute("/w/$slug/inbox/received/")({
  validateSearch: search,
  head: () => ({ meta: [{ title: "Received mail · Norbelys" }] }),
  component: ReceivedMail,
});
