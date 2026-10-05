import { InboxIcon } from "@hugeicons/core-free-icons";
import type { ThreadObject } from "@norbelys/sdk";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { z } from "zod";

import { ListTable } from "@/components/data-table";
import { PageBody } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { RelativeTime } from "@/components/time";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import {
  classificationOptions,
  LastMessage,
} from "@/features/inbox/classification";
import { InboxHeader } from "@/features/inbox/inbox-header";
import {
  CLASSIFICATIONS,
  THREAD_STATUSES,
  threadListQuery,
} from "@/features/inbox/queries";
import { ALL, MailboxFilter } from "@/features/messages/message-filters";
import { formatSubject } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

type StatusChoice = (typeof THREAD_STATUSES)[number] | "all";

const STATUS_OPTIONS: { label: string; value: StatusChoice }[] = [
  { label: "Open", value: "open" },
  { label: "Snoozed", value: "snoozed" },
  { label: "Archived", value: "archived" },
  { label: "All", value: "all" },
];

const CLASSIFICATION_OPTIONS = classificationOptions({
  label: "Any classification",
  value: ALL,
});

/** The subject, bold behind an accent dot while something arrived that nobody marked read. */
const SubjectCell = ({ thread }: { thread: ThreadObject }) => (
  <span className="flex min-w-0 items-center gap-2">
    <span
      aria-label={thread.unread ? "Unread" : undefined}
      className={cn(
        "size-1.5 shrink-0 rounded-full",
        thread.unread ? "bg-accent" : "bg-transparent"
      )}
      role={thread.unread ? "img" : undefined}
    />
    <span
      className={cn(
        "text-fg block max-w-[340px] truncate",
        thread.unread ? "font-bold" : null
      )}
    >
      {formatSubject(thread.subject)}
    </span>
  </span>
);

/**
 * Conversations with people, the latest activity first: our messages and their answers, one
 * thread per person and sender. Status, classification and mailbox narrow the list, as
 * `threads.list` does; the address keeps them.
 */
const Conversations = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const routeNavigate = Route.useNavigate();
  const { classification, connection_id, status = "open" } = Route.useSearch();
  const filters = {
    classification,
    connection_id,
    status: status === "all" ? undefined : status,
  };
  const narrowed = Boolean(classification ?? connection_id);
  return (
    <PageBody>
      <InboxHeader />
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <Segmented<StatusChoice>
            label="Status"
            onChange={(next) => {
              void routeNavigate({
                replace: true,
                search: (previous) => ({
                  ...previous,
                  status: next === "open" ? undefined : next,
                }),
              });
            }}
            options={STATUS_OPTIONS}
            value={status}
          />
          <Select
            className="w-52"
            label="Classification"
            onChange={(next) => {
              void routeNavigate({
                replace: true,
                search: (previous) => ({
                  ...previous,
                  classification: CLASSIFICATIONS.find(
                    (value) => value === next
                  ),
                }),
              });
            }}
            options={CLASSIFICATION_OPTIONS}
            value={classification ?? ALL}
          />
          <MailboxFilter
            onChange={(next) => {
              void routeNavigate({
                replace: true,
                search: (previous) => ({ ...previous, connection_id: next }),
              });
            }}
            value={connection_id}
          />
          {narrowed ? (
            <Button
              onClick={() => {
                void routeNavigate({
                  replace: true,
                  search: (previous) => ({
                    ...previous,
                    classification: undefined,
                    connection_id: undefined,
                  }),
                });
              }}
              variant="tertiary"
            >
              Clear filters
            </Button>
          ) : null}
        </div>
        <ListTable<ThreadObject>
          columns={[
            {
              render: (thread) => <SubjectCell thread={thread} />,
              header: "Subject",
              id: "subject",
            },
            {
              render: (thread) => (
                <span className="text-fg-2 block max-w-[260px] truncate">
                  {thread.participants.join(", ")}
                </span>
              ),
              header: "Participants",
              id: "participants",
            },
            {
              render: (thread) => <LastMessage thread={thread} />,
              header: "Last message",
              id: "last",
            },
            {
              render: (thread) => (
                <StatusBadge kind="thread" value={thread.status} />
              ),
              header: "Status",
              id: "status",
            },
            {
              render: (thread) => (
                <RelativeTime value={thread.last_activity_at} />
              ),
              header: "Activity",
              id: "activity",
            },
          ]}
          empty={
            narrowed || status !== "open"
              ? {
                  description: "No conversation matches these filters.",
                  icon: InboxIcon,
                  title: "No results",
                }
              : {
                  description:
                    "Replies to your campaigns and direct messages arrive here, classified as human replies, auto-replies or bounces.",
                  icon: InboxIcon,
                  illustration: "inbox",
                  title: "No open conversations",
                }
          }
          onRowClick={(thread) => {
            void navigate({
              params: { slug: workspace.slug, threadId: thread.id },
              to: "/w/$slug/inbox/$threadId",
            });
          }}
          query={threadListQuery(workspace, filters)}
          rowKey={(thread) => thread.id}
        />
      </div>
    </PageBody>
  );
};

// The filters as the address keeps them, named as the list filters they feed.
const search = z.object({
  classification: z.enum(CLASSIFICATIONS).optional(),
  connection_id: z.string().optional(),
  status: z.enum([...THREAD_STATUSES, "all"]).optional(),
});

export const Route = createFileRoute("/w/$slug/inbox/")({
  validateSearch: search,
  head: () => ({ meta: [{ title: "Inbox · Norbelys" }] }),
  component: Conversations,
});
