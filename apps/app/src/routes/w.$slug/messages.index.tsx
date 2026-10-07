import { Mail01Icon, MailSend01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { MessageObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { z } from "zod";

import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { RelativeTime } from "@/components/time";
import { Button } from "@/components/ui/button";
import {
  isFiltered,
  MessageFilterBar,
} from "@/features/messages/message-filters";
import { MessageStatus } from "@/features/messages/message-status";
import { MESSAGE_STATES, messageListQuery } from "@/features/messages/queries";
import type { MessageFilters } from "@/features/messages/queries";
import { formatSubject, humanize } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The primary action: the page that writes and queues a direct message. */
const SendMessageButton = () => {
  const workspace = useWorkspace();
  return (
    <Button
      nativeButton={false}
      render={
        <Link
          aria-label="Send message"
          params={{ slug: workspace.slug }}
          to="/w/$slug/messages/new"
        />
      }
      variant="primary"
    >
      <HugeiconsIcon icon={MailSend01Icon} />
      Send message
    </Button>
  );
};

/**
 * Every message the workspace sent or will send, newest first, narrowed by the filters `messages.list`
 * takes (kept in the address, so a filtered list can be linked); a row opens the message.
 */
const MessagesPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const routeNavigate = Route.useNavigate();
  const filters: MessageFilters = Route.useSearch();
  const filtered = isFiltered(filters);
  // Filters wait until there is something to filter: an empty workspace sees one clear invitation.
  const any = useQuery({
    queryFn: async ({ signal }) => {
      const page = await workspace.api.messages.list({ limit: 1 }, { signal });
      return page.data.length > 0;
    },
    queryKey: [workspace.id, "messages", "any"],
  });
  return (
    <PageBody>
      <PageHeader
        actions={canWrite(workspace) ? <SendMessageButton /> : null}
        title="Messages"
      />
      <div className="flex flex-col gap-2">
        {filtered || any.data ? (
          <MessageFilterBar
            filters={filters}
            onChange={(patch) => {
              void routeNavigate({
                replace: true,
                search: (previous) => ({ ...previous, ...patch }),
              });
            }}
          />
        ) : null}
        <ListTable<MessageObject>
          columns={[
            {
              render: (message) => (
                <span className="text-fg block max-w-[360px] truncate font-semibold">
                  {formatSubject(message.subject)}
                </span>
              ),
              header: "Subject",
              id: "subject",
            },
            {
              render: (message) => (
                <span className="text-fg-2 block max-w-[240px] truncate">
                  {message.to.join(", ")}
                </span>
              ),
              header: "To",
              id: "to",
            },
            {
              render: (message) => (
                <span className="text-fg-2 block max-w-[220px] truncate">
                  {message.from.email}
                </span>
              ),
              header: "From",
              id: "from",
            },
            {
              render: (message) => <MessageStatus message={message} />,
              header: "Status",
              id: "state",
            },
            {
              render: (message) => (
                <span className="text-fg-2">{humanize(message.kind)}</span>
              ),
              header: "Kind",
              id: "kind",
            },
            {
              render: (message) => <RelativeTime value={message.created_at} />,
              header: "Created",
              id: "created",
            },
          ]}
          empty={
            filtered
              ? {
                  description:
                    "No message matches these filters. Clear them to see every message.",
                  icon: Mail01Icon,
                  title: "No results",
                }
              : {
                  action: canWrite(workspace) ? (
                    <SendMessageButton />
                  ) : undefined,
                  description:
                    "Every message a campaign, a reply or the API sends appears here with its delivery history.",
                  icon: Mail01Icon,
                  illustration: "first-send",
                  title: "No messages yet",
                }
          }
          onRowClick={(message) => {
            void navigate({
              params: { messageId: message.id, slug: workspace.slug },
              to: "/w/$slug/messages/$messageId",
            });
          }}
          query={messageListQuery(workspace, filters)}
          rowKey={(message) => message.id}
        />
      </div>
    </PageBody>
  );
};

// The filters as the address keeps them, each named as the `messages.list` filter it feeds.
const search = z.object({
  campaign_id: z.string().optional(),
  connection_id: z.string().optional(),
  person_id: z.string().optional(),
  state: z.enum(MESSAGE_STATES).optional(),
  thread_id: z.string().optional(),
});

export const Route = createFileRoute("/w/$slug/messages/")({
  validateSearch: search,
  head: () => ({ meta: [{ title: "Messages · Norbelys" }] }),
  component: MessagesPage,
});
