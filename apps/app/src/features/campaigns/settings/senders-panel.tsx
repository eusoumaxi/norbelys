import type { ConnectionObject, OnSenderRemoved } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";

import { Problem } from "@/components/problem";
import { SettingsPanel } from "@/components/settings-layout";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldError } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { Skeleton } from "@/components/ui/skeleton";
import { ON_SENDER_REMOVED } from "@/features/campaigns/format";
import { identityLabel, poolOf } from "@/features/campaigns/pool";
import { sendersQuery } from "@/features/campaigns/queries";
import type { SenderIdentity } from "@/features/campaigns/queries";
import type { PanelProps } from "@/features/campaigns/settings/schedule-panel";
import { parseTags } from "@/features/campaigns/settings/settings-draft";
import { providerLabel } from "@/features/mailboxes/providers";
import { FormField } from "@/lib/form";
import { plural } from "@/lib/format";
import { problemAt } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The identities of each connection, in the order the connections were listed. */
const byConnection = (senders: SenderIdentity[]) => {
  const groups = new Map<
    string,
    { connection: ConnectionObject; senders: SenderIdentity[] }
  >();
  for (const sender of senders) {
    const group = groups.get(sender.connection.id);
    if (group) {
      group.senders.push(sender);
    } else {
      groups.set(sender.connection.id, {
        connection: sender.connection,
        senders: [sender],
      });
    }
  }
  return [...groups.values()];
};

/** Every identity of every mailbox, each with a box that names it in the pool. */
const IdentityPicker = ({ draft, set }: Omit<PanelProps, "errors">) => {
  const workspace = useWorkspace();
  const senders = useQuery(sendersQuery(workspace));
  if (senders.isError) {
    return (
      <Problem
        error={senders.error}
        onRetry={() => {
          void senders.refetch();
        }}
      />
    );
  }
  if (!senders.data) {
    return <Skeleton className="h-24 w-full" />;
  }
  if (senders.data.length === 0) {
    return (
      <p className="text-fg-3 border-line rounded-sm border p-4 text-sm">
        No mailbox is connected yet. Connect one under Mailboxes, and its
        addresses appear here.
      </p>
    );
  }
  const toggle = (id: string, on: boolean) =>
    set({
      identity_ids: on
        ? [...draft.identity_ids, id]
        : draft.identity_ids.filter((x) => x !== id),
    });
  return (
    <div className="border-line divide-line max-h-[360px] divide-y overflow-y-auto rounded-sm border">
      {byConnection(senders.data).map(({ connection, senders: list }) => (
        <div key={connection.id}>
          <div className="bg-chrome flex min-h-9 flex-wrap items-center gap-2 px-3 py-2 text-xs">
            <span className="text-fg min-w-0 font-semibold break-all">
              {connection.account.email}
            </span>
            <span className="text-fg-3">
              {providerLabel(connection.provider)}
            </span>
            <StatusBadge kind="connection" value={connection.status} />
          </div>
          {list.map((sender) => (
            <label
              className="hover:bg-hover flex min-h-10 cursor-pointer flex-wrap items-center gap-3 px-3 py-2 text-sm"
              key={sender.identity.id}
            >
              <Checkbox
                checked={draft.identity_ids.includes(sender.identity.id)}
                onCheckedChange={(checked) =>
                  toggle(sender.identity.id, checked)
                }
              />
              <span className="text-fg min-w-0 flex-1 truncate">
                {identityLabel(sender)}
              </span>
              {sender.identity.enabled ? null : (
                <Badge tone="muted">Disabled</Badge>
              )}
              {sender.identity.tags.map((tag) => (
                <Badge key={tag}>{tag}</Badge>
              ))}
            </label>
          ))}
        </div>
      ))}
    </div>
  );
};

/** Who the pool holds with the senders and tags chosen so far. */
const PoolPreview = ({ draft }: { draft: PanelProps["draft"] }) => {
  const workspace = useWorkspace();
  const senders = useQuery(sendersQuery(workspace));
  if (!senders.data) {
    return null;
  }
  const pool = poolOf(
    { identity_ids: draft.identity_ids, tags: parseTags(draft.tags) },
    senders.data
  );
  return (
    <p className="text-fg-2 text-sm break-words">
      {pool.length > 0
        ? `Sends from ${plural(pool.length, "address", "addresses")}, taking turns: ${pool.map(identityLabel).join(", ")}.`
        : "No address yet: choose at least one, or nothing can be sent."}
    </p>
  );
};

const REMOVAL_OPTIONS: { label: string; value: OnSenderRemoved }[] = [
  { label: ON_SENDER_REMOVED.reassign, value: "reassign" },
  { label: ON_SENDER_REMOVED.stop, value: "stop" },
];

/**
 * Who sends the campaign: identities named one by one, tags whose enabled identities join, and
 * what happens to a conversation whose sender leaves the pool. Each conversation keeps its
 * sender for its follow-ups.
 */
export const SendersPanel = ({ draft, errors, set }: PanelProps) => (
  <SettingsPanel
    description="The campaign's emails go out from these addresses, taking turns. Each person hears from the same address in every follow-up."
    title="Who sends it"
  >
    <FormField label="Addresses">
      <IdentityPicker draft={draft} set={set} />
      {problemAt(errors, "senders.identity_ids") ? (
        <FieldError>{problemAt(errors, "senders.identity_ids")}</FieldError>
      ) : null}
    </FormField>
    <FormField
      description="Addresses with one of these tags join by themselves, including ones you add later. Separate tags with commas."
      htmlFor="senders-tags"
      label="Also every address tagged"
      optional
      problem={problemAt(errors, "senders.tags")}
    >
      <Input
        aria-invalid={Boolean(problemAt(errors, "senders.tags"))}
        id="senders-tags"
        onChange={(event) => set({ tags: event.target.value })}
        placeholder="outbound, emea"
        value={draft.tags}
      />
    </FormField>
    <PoolPreview draft={draft} />
    <FormField
      description={
        draft.on_sender_removed === "reassign"
          ? "Another of the campaign's addresses sends the next email, in a new thread."
          : "That person gets no more emails from this campaign."
      }
      label="If an address is removed"
      problem={problemAt(errors, "senders.on_sender_removed")}
    >
      <Segmented
        label="If an address is removed"
        onChange={(value) => set({ on_sender_removed: value })}
        options={REMOVAL_OPTIONS}
        value={draft.on_sender_removed}
      />
    </FormField>
  </SettingsPanel>
);
