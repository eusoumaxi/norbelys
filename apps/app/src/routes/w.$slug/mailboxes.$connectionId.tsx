import type { ConnectionObject } from "@norbelys/sdk";
import { useQueryClient, useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Outlet } from "@tanstack/react-router";
import { useEffect } from "react";
import { toast } from "sonner";

import { PageBody, PageHeader } from "@/components/page";
import { TabLinks } from "@/components/ui/tabs";
import { ConsentRefused, useConsentReturn } from "@/features/mailboxes/consent";
import {
  MailboxActions,
  MailboxNotices,
} from "@/features/mailboxes/mailbox-header";
import { ConnectionHealth } from "@/features/mailboxes/parts";
import { kindLabel } from "@/features/mailboxes/providers";
import { connectionQuery, connectionsKey } from "@/features/mailboxes/queries";
import { formatRelative } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/**
 * Where a new consent (Reconnect) comes back: a reconnected mailbox is said and read again; a
 * refused one stays in the address until dismissed.
 */
const useReconnectLanding = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const consent = useConsentReturn();
  const { clear, connectionId } = consent;
  useEffect(() => {
    if (!connectionId) {
      return;
    }
    toast.success("Mailbox reconnected. Its check is running.", {
      id: connectionId,
    });
    void queryClient.invalidateQueries({ queryKey: connectionsKey(workspace) });
    clear();
  }, [clear, connectionId, queryClient, workspace]);
  return consent;
};

/** The line under the address: whether it works, what kind of account it is, its last check. */
const MailboxLine = ({ connection }: { connection: ConnectionObject }) => (
  <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
    <ConnectionHealth connection={connection} />
    {/* Each dot travels with what follows it, so a wrapped line never ends on one. */}
    <span className="whitespace-nowrap">
      <span aria-hidden className="text-fg-4 mr-2">
        ·
      </span>
      {kindLabel(connection.provider)}
    </span>
    {connection.checked_at ? (
      <span className="text-fg-3 whitespace-nowrap">
        <span aria-hidden className="text-fg-4 mr-2">
          ·
        </span>
        Checked {formatRelative(connection.checked_at).toLowerCase()}
      </span>
    ) : null}
  </span>
);

/**
 * One mailbox: its address with whether it works, the notices that need a person (each with the
 * action that fixes it), and its two sections as tabs: the overview (how it sends, its senders
 * and their signatures) and its settings. It reads itself again every few seconds while its
 * check runs.
 */
const MailboxPage = () => {
  const workspace = useWorkspace();
  const { connectionId } = Route.useParams();
  const { data: connection } = useSuspenseQuery(
    connectionQuery(workspace, connectionId)
  );
  const consent = useReconnectLanding();
  const params = { connectionId, slug: workspace.slug };
  return (
    <PageBody>
      <div className="max-w-[960px]">
        <PageHeader
          actions={<MailboxActions connection={connection} />}
          back={{
            label: "Mailboxes",
            link: {
              params: { slug: workspace.slug },
              to: "/w/$slug/mailboxes",
            },
          }}
          compact
          subtitle={<MailboxLine connection={connection} />}
          title={<span className="break-all">{connection.account.email}</span>}
        />
        {consent.error ? (
          <ConsentRefused
            description={consent.error.description}
            onDismiss={() => consent.clear()}
            title="The mailbox was not reconnected"
          />
        ) : null}
        <MailboxNotices connection={connection} />
        <div className="flex flex-col gap-6">
          <TabLinks
            tabs={[
              {
                exact: true,
                label: "Overview",
                link: { params, to: "/w/$slug/mailboxes/$connectionId" },
              },
              {
                label: "Settings",
                link: {
                  params,
                  to: "/w/$slug/mailboxes/$connectionId/settings",
                },
              },
            ]}
          />
          <Outlet />
        </div>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/mailboxes/$connectionId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      connectionQuery(context.workspace, params.connectionId)
    ),
  head: ({ loaderData }) => ({
    meta: [{ title: `${loaderData?.account.email ?? "Mailbox"} · Norbelys` }],
  }),
  component: MailboxPage,
});
