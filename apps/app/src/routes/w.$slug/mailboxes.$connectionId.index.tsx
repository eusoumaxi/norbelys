import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";

import { connectionQuery } from "@/features/mailboxes/queries";
import { Senders } from "@/features/mailboxes/senders";
import { SendingSummary } from "@/features/mailboxes/summary";
import { useWorkspace } from "@/lib/workspace";

/**
 * A mailbox's overview: how it sends (today's count against what it may send, its pace, when,
 * its warm-up, its replies), then its senders, each with the name people see and its signature.
 */
const MailboxOverview = () => {
  const workspace = useWorkspace();
  const { connectionId } = Route.useParams();
  const { data: connection } = useSuspenseQuery(
    connectionQuery(workspace, connectionId)
  );
  return (
    <div className="flex flex-col gap-8">
      <SendingSummary connection={connection} />
      <Senders connection={connection} key={connection.id} />
    </div>
  );
};

export const Route = createFileRoute("/w/$slug/mailboxes/$connectionId/")({
  component: MailboxOverview,
});
