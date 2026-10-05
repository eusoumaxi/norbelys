import { Add01Icon, UnavailableIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { SuppressionObject } from "@norbelys/sdk";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import {
  suppressionListQuery,
  suppressionsKey,
} from "@/features/suppressions/queries";
import { SuppressDialog } from "@/features/suppressions/suppress-dialog";
import { useAction } from "@/lib/actions";
import { formatRelative, humanize } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/**
 * Who or what suppressed an address, in plain words. A source this dashboard does not know yet
 * shows as its own words.
 */
const SOURCES: Record<string, string> = {
  arf: "Complaint report",
  dsn: "Bounce message",
  inbound_notice: "Notice in the inbox",
  manual: "Added by hand",
  provider_api: "Sending provider",
  provider_webhook: "Sending provider",
  smtp: "Recipient's server",
  unsubscribe: "Unsubscribe link",
};

/** "Suppress addresses": the page's one action, for people who may change the audience. */
const SuppressButton = ({ onClick }: { onClick: () => void }) => (
  <Button onClick={onClick} variant="primary">
    <HugeiconsIcon icon={Add01Icon} />
    Suppress addresses
  </Button>
);

/**
 * Addresses nothing of the workspace is sent to again, newest first (`suppressions.list`), with
 * why and who said so. Unsubscribes, bounces and complaints add them on their own; a person
 * suppresses more in a dialog (typed, pasted or from a CSV file), and may lift only those added
 * by hand (`suppressions.delete`, confirmed first): the API keeps the others, which evidence made.
 */
const Suppressions = () => {
  const workspace = useWorkspace();
  const act = useAction();
  const editable = canWrite(workspace);
  const [creating, setCreating] = useState(false);
  // The suppression a confirmation is about; kept while the dialog closes, so its words stay.
  const [lifting, setLifting] = useState<SuppressionObject | null>(null);
  const [confirming, setConfirming] = useState(false);
  const action = editable ? (
    <SuppressButton onClick={() => setCreating(true)} />
  ) : null;
  return (
    <PageBody>
      <PageHeader
        actions={action}
        subtitle="Nothing from this workspace reaches these addresses. Unsubscribes, bounces and complaints add them on their own; you can add more, and lift the ones you added."
        title="Suppressions"
      />
      <ListTable<SuppressionObject>
        columns={[
          {
            render: (s) => <span className="text-fg font-bold">{s.email}</span>,
            header: "Address",
            id: "email",
          },
          {
            render: (s) => (
              <StatusBadge dot={false} kind="suppression" value={s.reason} />
            ),
            header: "Reason",
            id: "reason",
          },
          {
            render: (s) => SOURCES[s.source] ?? humanize(s.source),
            header: "Source",
            id: "source",
          },
          {
            render: (s) => formatRelative(s.created_at),
            header: "Since",
            id: "created",
          },
          {
            render: (s) =>
              editable && s.reason === "manual" ? (
                <RowMenu>
                  <DropdownMenuItem
                    className="text-error-fg"
                    onClick={() => {
                      setLifting(s);
                      setConfirming(true);
                    }}
                  >
                    Lift suppression
                  </DropdownMenuItem>
                </RowMenu>
              ) : null,
            className: "w-[62px]",
            header: "",
            id: "menu",
          },
        ]}
        empty={{
          action,
          description:
            "Unsubscribes, bounces and spam complaints appear here on their own. You can also suppress addresses yourself, one at a time or from a CSV file.",
          icon: UnavailableIcon,
          title: "No suppressed addresses",
        }}
        query={suppressionListQuery(workspace)}
        rowKey={(s) => s.id}
      />
      <SuppressDialog onOpenChange={setCreating} open={creating} />
      <ConfirmDialog
        confirmLabel="Lift suppression"
        danger
        description={`${lifting?.email ?? "This address"} can receive mail from this workspace again: campaigns and messages may reach it.`}
        onConfirm={async () => {
          if (lifting) {
            await act(
              "Suppression lifted",
              () => workspace.api.suppressions.delete(lifting.id),
              suppressionsKey(workspace)
            );
          }
        }}
        onOpenChange={setConfirming}
        open={confirming}
        title="Lift this suppression?"
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/suppressions")({
  head: () => ({ meta: [{ title: "Suppressions · Norbelys" }] }),
  component: Suppressions,
});
