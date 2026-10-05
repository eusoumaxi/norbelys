import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";

import { MailboxSettings } from "@/features/mailboxes/mailbox-settings";
import { connectionQuery } from "@/features/mailboxes/queries";
import { useWorkspace } from "@/lib/workspace";

/** A mailbox's settings: its pace, window, quota scope, folders, servers and credentials. */
const MailboxSettingsPage = () => {
  const workspace = useWorkspace();
  const { connectionId } = Route.useParams();
  const { data: connection } = useSuspenseQuery(
    connectionQuery(workspace, connectionId)
  );
  return <MailboxSettings connection={connection} key={connection.id} />;
};

export const Route = createFileRoute(
  "/w/$slug/mailboxes/$connectionId/settings"
)({
  head: () => ({ meta: [{ title: "Mailbox settings · Norbelys" }] }),
  component: MailboxSettingsPage,
});
