import { Refresh01Icon, ServerStack01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { DeliveryEventObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import {
  createStandardSchemaV1,
  debounce,
  parseAsString,
  useQueryState,
} from "nuqs";
import { useEffect, useState } from "react";

import { Dash, ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Select } from "@/components/ui/select";
import { campaignOptionsQuery } from "@/features/campaigns/queries";
import { CONFIDENCE, isKind, KIND_OPTIONS } from "@/features/logs/kinds";
import {
  deliveryEventListQuery,
  deliveryEventsKey,
} from "@/features/logs/queries";
import { FilterChip } from "@/features/messages/message-filters";
import { formatTimestamp, humanize, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** How long typing pauses before the recipient filter applies: it matches whole addresses. */
const SETTLE_MS = 400;

/** `value` once it has stopped changing for a moment, so typing an address costs one request. */
const useSettled = (value: string): string => {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), SETTLE_MS);
    return () => clearTimeout(timer);
  }, [value]);
  return settled;
};

// Declared once for nuqs (state) and the router (typed links from other pages).
const search = {
  campaign: parseAsString,
  kind: parseAsString,
  message: parseAsString,
  person: parseAsString,
  recipient: parseAsString.withDefault(""),
};

/** Why it happened (the category), and how far the report can be trusted, from where. */
const Cause = ({ event }: { event: DeliveryEventObject }) => (
  <span className="flex min-w-0 flex-col">
    <span className="text-fg">{humanize(event.category)}</span>
    <span
      className="text-fg-3 text-xs"
      title={CONFIDENCE[event.confidence] ?? undefined}
    >
      {humanize(event.confidence)} · {humanize(event.source)}
    </span>
  </span>
);

/** The provider's own words: the enhanced status code, then the diagnostic. */
const Diagnostic = ({ event }: { event: DeliveryEventObject }) => {
  if (!event.diagnostic && !event.enhanced_status) {
    return <Dash />;
  }
  return (
    <span
      className="text-fg-2 block max-w-[340px] truncate text-xs"
      title={event.diagnostic ?? undefined}
    >
      {event.enhanced_status ? (
        <code className="text-fg mr-1.5 font-mono">
          {event.enhanced_status}
        </code>
      ) : null}
      {event.diagnostic}
    </span>
  );
};

/** The filters of the logs: a recipient, a kind and a campaign, as the API accepts them. */
const Filters = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const campaigns = useQuery(campaignOptionsQuery(workspace));
  const [recipient, setRecipient] = useQueryState(
    "recipient",
    search.recipient.withOptions({ limitUrlUpdates: debounce(300) })
  );
  const [kind, setKind] = useQueryState("kind", search.kind);
  const [campaign, setCampaign] = useQueryState("campaign", search.campaign);
  return (
    <div className="flex flex-wrap items-center gap-2">
      <SearchInput
        className="min-w-60 flex-1"
        label="Recipient"
        onChange={(value) => {
          void setRecipient(value || null);
        }}
        placeholder="Recipient address, exactly (any case)"
        value={recipient}
      />
      <Select
        className="w-44"
        label="Kind"
        onChange={(value) => {
          void setKind(value === "all" ? null : value);
        }}
        options={KIND_OPTIONS}
        value={kind && isKind(kind) ? kind : "all"}
      />
      <Select
        className="w-56"
        label="Campaign"
        onChange={(value) => {
          void setCampaign(value === "all" ? null : value);
        }}
        options={[
          { label: "All campaigns", value: "all" },
          ...(campaigns.data?.data ?? []).map((choice) => ({
            label: choice.name,
            value: choice.id,
          })),
        ]}
        value={campaign ?? "all"}
      />
      <Button
        onClick={() => {
          void queryClient.invalidateQueries({
            queryKey: deliveryEventsKey(workspace),
          });
        }}
        variant="secondary"
      >
        <HugeiconsIcon icon={Refresh01Icon} />
        Refresh
      </Button>
    </div>
  );
};

/**
 * The workspace's delivery logs: every piece of evidence about a message after it left (an
 * acceptance, a deferral, a delivery, a bounce, a complaint), newest first, filtered as the API
 * allows. Each row opens its message.
 */
const LogsPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [recipient, setRecipient] = useQueryState(
    "recipient",
    search.recipient
  );
  const [kind] = useQueryState("kind", search.kind);
  const [campaign] = useQueryState("campaign", search.campaign);
  const [message, setMessage] = useQueryState("message", search.message);
  const [person, setPerson] = useQueryState("person", search.person);
  const applied = useSettled(recipient.trim());
  const filters = {
    campaign_id: campaign ?? undefined,
    kind: kind && isKind(kind) ? kind : undefined,
    message_id: message ?? undefined,
    person_id: person ?? undefined,
    recipient: applied || undefined,
  };
  const filtered = Object.values(filters).some(Boolean);
  return (
    <PageBody>
      <PageHeader title="Delivery logs" />
      <p className="text-fg-2 mb-4 max-w-[860px] text-sm">
        Evidence about each message after it left. The kind is what happened;
        the category says why, from a closed list (invalid recipient, mailbox
        full, policy…); the confidence says how far the report can be trusted:
        authenticated by the server that reported it, corroborated by our
        records, inferred from a partial match, or a person&apos;s words sent to
        review. A deferral is temporary; only a bounce or a rejection is final.
      </p>
      <div className="flex flex-col gap-3">
        <Filters />
        {message || person ? (
          <div className="flex flex-wrap gap-2">
            {message ? (
              <FilterChip
                label="Message"
                onClear={() => {
                  void setMessage(null);
                }}
                value={message}
              />
            ) : null}
            {person ? (
              <FilterChip
                label="Person"
                onClear={() => {
                  void setPerson(null);
                }}
                value={person}
              />
            ) : null}
          </div>
        ) : null}
        <ListTable<DeliveryEventObject>
          columns={[
            {
              render: (e) => (
                <time
                  className="text-fg-2 whitespace-nowrap tabular-nums"
                  dateTime={e.observed_at}
                  title={`Observed by its source; received ${formatTimestamp(e.received_at)}, recorded ${formatTimestamp(e.processed_at)}`}
                >
                  {formatTimestamp(e.observed_at)}
                </time>
              ),
              header: "Observed",
              id: "observed",
            },
            {
              render: (e) => <StatusBadge kind="event" value={e.kind} />,
              header: "Kind",
              id: "kind",
            },
            {
              render: (e) =>
                e.recipient ? (
                  <span className="text-fg block max-w-[220px] truncate">
                    {e.recipient}
                  </span>
                ) : (
                  <Dash />
                ),
              header: "Recipient",
              id: "recipient",
            },
            {
              render: (e) => <Cause event={e} />,
              header: "Category",
              id: "category",
            },
            {
              render: (e) => <Diagnostic event={e} />,
              header: "Diagnostic",
              id: "diagnostic",
            },
            {
              render: (e) =>
                e.message_id ? (
                  <Link
                    className="text-link hover:text-link-hover font-mono text-xs"
                    params={{ messageId: e.message_id, slug: workspace.slug }}
                    to="/w/$slug/messages/$messageId"
                  >
                    {shortId(e.message_id)}
                  </Link>
                ) : (
                  <Dash />
                ),
              header: "Message",
              id: "message",
            },
            {
              render: (e) => (
                <RowMenu>
                  {e.message_id ? (
                    <DropdownMenuItem
                      onClick={() => {
                        void setMessage(e.message_id ?? null);
                      }}
                    >
                      Only this message
                    </DropdownMenuItem>
                  ) : null}
                  {e.recipient ? (
                    <DropdownMenuItem
                      onClick={() => {
                        void setRecipient(e.recipient ?? null);
                      }}
                    >
                      Only this recipient
                    </DropdownMenuItem>
                  ) : null}
                  <CopyIdItem id={e.id} noun="event" />
                </RowMenu>
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            description: filtered
              ? "No delivery event matches these filters."
              : "Acceptances, deliveries, bounces and complaints appear here as providers report them.",
            icon: ServerStack01Icon,
            title: filtered ? "No results" : "No delivery events yet",
          }}
          onRowClick={(e) => {
            if (e.message_id) {
              void navigate({
                params: { messageId: e.message_id, slug: workspace.slug },
                to: "/w/$slug/messages/$messageId",
              });
            }
          }}
          query={deliveryEventListQuery(workspace, filters)}
          rowKey={(e) => e.id}
        />
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/logs")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Delivery logs · Norbelys" }] }),
  component: LogsPage,
});
