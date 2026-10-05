import { Add01Icon, DashboardSpeed01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { QuotaScopeObject } from "@norbelys/sdk";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CopyButton } from "@/components/copy";
import { Dash, ListTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Select } from "@/components/ui/select";
import { holdUntil, useNow } from "@/features/mailboxes/parts";
import {
  PROVIDER_IDS,
  PROVIDERS,
  providerLabel,
} from "@/features/mailboxes/providers";
import {
  quotaScopeListQuery,
  quotaScopesKey,
} from "@/features/mailboxes/queries";
import { QuotaScopeDialog } from "@/features/mailboxes/quota-scope-dialog";
import { ALL } from "@/features/messages/message-filters";
import { useAction } from "@/lib/actions";
import { formatCount, formatDateTime } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

const PROVIDER_FILTER = [
  { label: "All providers", value: ALL },
  ...PROVIDER_IDS.map((id) => ({ label: PROVIDERS[id].label, value: id })),
];

/** A daily limit, or a dash when the scope sets none. */
const limit = (value: number | null | undefined) =>
  value === null || value === undefined ? <Dash /> : formatCount(value);

/** `14 recipients per 1 s`, or a dash when the scope has no short window. */
const windowCell = (scope: QuotaScopeObject) =>
  scope.window_limit && scope.window_seconds ? (
    `${formatCount(scope.window_limit)} ${scope.window_unit ?? "units"} per ${formatCount(scope.window_seconds)} s`
  ) : (
    <Dash />
  );

/** Whether the provider paused the scope's account, until when and why. */
const StateCell = ({ scope }: { scope: QuotaScopeObject }) => {
  const paused = holdUntil(scope.paused_until, useNow());
  if (paused) {
    return (
      <span className="flex min-w-0 flex-col gap-0.5">
        <span>
          <Badge dot tone="warning">
            Paused until {formatDateTime(paused)}
          </Badge>
        </span>
        {scope.paused_detail ? (
          <span
            className="text-fg-3 truncate text-xs"
            title={scope.paused_detail}
          >
            {scope.paused_detail}
          </span>
        ) : null}
      </span>
    );
  }
  return <span className="text-fg-3">Not paused</span>;
};

/** An SES scope's webhook: the URL its account's SNS topic posts to. */
const WebhookCell = ({ scope }: { scope: QuotaScopeObject }) =>
  scope.webhook ? (
    <span className="flex min-w-0 items-center gap-1.5">
      <code
        className="text-fg-2 min-w-0 truncate font-mono text-xs"
        title={scope.webhook.url}
      >
        {scope.webhook.url}
      </code>
      <CopyButton label="Copy webhook URL" value={scope.webhook.url} />
    </span>
  ) : (
    <Dash />
  );

/**
 * The workspace's quota scopes: the limits of provider accounts it owns (an SES account and
 * Region, a Microsoft tenant, a relay account), each shared by the connections that name it.
 * The table shows each scope's daily limits, its short window, whether its provider paused it
 * (until when and why) and an SES scope's webhook; scopes are created, edited and deleted here.
 */
const QuotasPage = () => {
  const workspace = useWorkspace();
  const action = useAction();
  const [provider, setProvider] = useState(ALL);
  const [editing, setEditing] = useState<QuotaScopeObject | null>(null);
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<QuotaScopeObject | null>(null);
  const viewer = !canWrite(workspace);

  const newButton = (
    <Button
      disabled={viewer}
      onClick={() => setCreating(true)}
      variant="primary"
    >
      <HugeiconsIcon icon={Add01Icon} />
      New quota scope
    </Button>
  );

  return (
    <PageBody>
      <PageHeader actions={newButton} title="Sending limits" />
      <div className="flex flex-col gap-3">
        <p className="text-fg-2 max-w-[800px] text-sm">
          A quota scope holds the limits of a provider account you own, shared
          by every connection that names it, so the sender stays under them
          across mailboxes. When the provider answers that the shared limit is
          reached, the scope pauses for a while, for every connection at once.
          Each connection keeps its own daily limit too.
        </p>
        <div className="w-full max-w-[240px]">
          <Select
            label="Provider"
            onChange={setProvider}
            options={PROVIDER_FILTER}
            value={provider}
          />
        </div>
        <ListTable<QuotaScopeObject>
          columns={[
            {
              header: "Account",
              id: "scope_key",
              render: (s) => (
                <NameCell icon={DashboardSpeed01Icon}>
                  <span className="font-mono">{s.scope_key}</span>
                </NameCell>
              ),
            },
            {
              header: "Provider",
              id: "provider",
              render: (s) => providerLabel(s.provider),
            },
            {
              header: "Messages a day",
              id: "messages",
              render: (s) => limit(s.messages_per_day),
            },
            {
              header: "Recipients a day",
              id: "recipients",
              render: (s) => limit(s.recipients_per_day),
            },
            { header: "Short window", id: "window", render: windowCell },
            {
              header: "State",
              id: "state",
              render: (s) => <StateCell scope={s} />,
            },
            {
              className: "max-w-[240px]",
              header: "Webhook",
              id: "webhook",
              render: (s) => <WebhookCell scope={s} />,
            },
            {
              className: "w-[62px]",
              header: "",
              id: "menu",
              render: (s) => (
                <RowMenu>
                  <DropdownMenuItem
                    disabled={viewer}
                    onClick={() => setEditing(s)}
                  >
                    Edit limits
                  </DropdownMenuItem>
                  <CopyIdItem id={s.id} noun="scope" />
                  <DropdownMenuItem
                    className="text-error-fg"
                    disabled={viewer}
                    onClick={() => setDeleting(s)}
                  >
                    Delete
                  </DropdownMenuItem>
                </RowMenu>
              ),
            },
          ]}
          empty={{
            action: newButton,
            description:
              "A quota scope is needed for every SES connection (its account and Region), and helps any provider account several connections share.",
            icon: DashboardSpeed01Icon,
            title: "No quota scopes",
          }}
          onRowClick={viewer ? undefined : setEditing}
          query={quotaScopeListQuery(
            workspace,
            provider === ALL ? "" : provider
          )}
          rowKey={(s) => s.id}
        />
      </div>
      <QuotaScopeDialog onOpenChange={setCreating} open={creating} />
      <QuotaScopeDialog
        onOpenChange={(open) => {
          if (!open) {
            setEditing(null);
          }
        }}
        open={editing !== null}
        scope={editing}
      />
      <ConfirmDialog
        confirmLabel="Delete scope"
        danger
        description={
          <>
            {deleting?.scope_key} and its ledger are deleted; the connections
            that name it keep running without a shared limit. The API refuses
            while SES connections, archived ones included, name it.
          </>
        }
        onConfirm={() =>
          deleting
            ? action(
                "Quota scope deleted",
                () => workspace.api.quotaScopes.delete(deleting.id),
                quotaScopesKey(workspace)
              )
            : undefined
        }
        onOpenChange={(open) => {
          if (!open) {
            setDeleting(null);
          }
        }}
        open={deleting !== null}
        title="Delete this quota scope?"
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/quotas")({
  head: () => ({ meta: [{ title: "Sending limits · Norbelys" }] }),
  component: QuotasPage,
});
