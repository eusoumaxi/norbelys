import { Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQuery } from "@tanstack/react-query";

import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import type { SelectOption } from "@/components/ui/select";
import { campaignOptionsQuery } from "@/features/campaigns/queries";
import { mailboxOptionsQuery } from "@/features/mailboxes/queries";
import { MESSAGE_STATES } from "@/features/messages/queries";
import type { MessageFilters } from "@/features/messages/queries";
import { PersonPicker } from "@/features/people/person-picker";
import { personQuery } from "@/features/people/queries";
import { shortId } from "@/lib/format";
import { statusLabel } from "@/lib/status";
import { useWorkspace } from "@/lib/workspace";

/** The choice that removes a filter; the API's ids never take this value. */
export const ALL = "all";

/**
 * The options of a filter whose choices load from the API: "all" first, then the loaded ones, and
 * the current value even when it is not among them (a link may name a campaign past the first 100).
 */
const withCurrent = (
  all: string,
  loaded: SelectOption[],
  current: string | undefined
): SelectOption[] => [
  { label: all, value: ALL },
  ...loaded,
  ...(current && !loaded.some((option) => option.value === current)
    ? [{ label: shortId(current), value: current }]
    : []),
];

/**
 * A choice of one of the workspace's mailboxes (its first 100), or all of them: the
 * `connection_id` filter the message, thread and inbound lists share.
 */
export const MailboxFilter = ({
  onChange,
  value,
}: {
  onChange: (value: string | undefined) => void;
  value: string | undefined;
}) => {
  const workspace = useWorkspace();
  const mailboxes = useQuery(mailboxOptionsQuery(workspace));
  const loaded = (mailboxes.data?.data ?? []).map((mailbox) => ({
    label: mailbox.account.email,
    value: mailbox.id,
  }));
  return (
    <Select
      className="w-56"
      label="Mailbox"
      onChange={(next) => onChange(next === ALL ? undefined : next)}
      options={withCurrent("All mailboxes", loaded, value)}
      value={value ?? ALL}
    />
  );
};

/** A choice of one of the workspace's 100 newest campaigns, or all of them. */
const CampaignFilter = ({
  onChange,
  value,
}: {
  onChange: (value: string | undefined) => void;
  value: string | undefined;
}) => {
  const workspace = useWorkspace();
  const campaigns = useQuery(campaignOptionsQuery(workspace));
  const loaded = (campaigns.data?.data ?? []).map((campaign) => ({
    label: campaign.name,
    value: campaign.id,
  }));
  return (
    <Select
      className="w-56"
      label="Campaign"
      onChange={(next) => onChange(next === ALL ? undefined : next)}
      options={withCurrent("All campaigns", loaded, value)}
      value={value ?? ALL}
    />
  );
};

const STATE_OPTIONS: SelectOption[] = [
  { label: "Any status", value: ALL },
  ...MESSAGE_STATES.map((state) => ({
    label: statusLabel("message", state),
    value: state,
  })),
];

/**
 * The person whose messages to show, picked by name or address (the API filters by their id). A
 * person named by a link is read once, so the picker shows who it is rather than an id.
 */
const PersonFilter = ({
  onChange,
  value,
}: {
  onChange: (value: string | undefined) => void;
  value: string | undefined;
}) => {
  const workspace = useWorkspace();
  const person = useQuery({
    ...personQuery(workspace, value ?? ""),
    enabled: Boolean(value),
  });
  return (
    <div className="w-64">
      <PersonPicker
        onChange={(picked) => onChange(picked?.id)}
        placeholder="Any person"
        value={value ? (person.data ?? null) : null}
      />
    </div>
  );
};

/** A filter only a link sets (a thread's messages), shown as a pill that removes it. */
export const FilterChip = ({
  label,
  onClear,
  value,
}: {
  label: string;
  onClear: () => void;
  value: string;
}) => (
  <span className="border-line bg-chrome text-fg-2 flex h-8 items-center gap-2 rounded-sm border pr-1 pl-3 text-sm">
    {label}
    <span className="text-fg font-mono text-xs">{shortId(value)}</span>
    <button
      aria-label={`Remove the ${label.toLowerCase()} filter`}
      className="text-icon hover:bg-hover focus-visible:outline-focus flex size-6 cursor-pointer items-center justify-center rounded-sm outline-none focus-visible:outline-1"
      onClick={onClear}
      type="button"
    >
      <HugeiconsIcon className="size-3.5" icon={Cancel01Icon} />
    </button>
  </span>
);

/** Whether any filter narrows the list. */
export const isFiltered = (filters: MessageFilters): boolean =>
  Object.values(filters).some(Boolean);

/**
 * The message list's filters in one row above it: state, campaign, mailbox and person, each a
 * `messages.list` filter; a thread set by a link shows as a pill. `onChange` gets the members to
 * change (`undefined` removes one).
 */
export const MessageFilterBar = ({
  filters,
  onChange,
}: {
  filters: MessageFilters;
  onChange: (patch: MessageFilters) => void;
}) => (
  <div className="flex flex-wrap items-center gap-2">
    <Select
      className="w-40"
      label="State"
      onChange={(next) =>
        onChange({
          state: MESSAGE_STATES.find((state) => state === next),
        })
      }
      options={STATE_OPTIONS}
      value={filters.state ?? ALL}
    />
    <CampaignFilter
      onChange={(campaign_id) => onChange({ campaign_id })}
      value={filters.campaign_id}
    />
    <MailboxFilter
      onChange={(connection_id) => onChange({ connection_id })}
      value={filters.connection_id}
    />
    <PersonFilter
      onChange={(person_id) => onChange({ person_id })}
      value={filters.person_id}
    />
    {filters.thread_id ? (
      <FilterChip
        label="Thread"
        onClear={() => onChange({ thread_id: undefined })}
        value={filters.thread_id}
      />
    ) : null}
    {isFiltered(filters) ? (
      <Button
        onClick={() =>
          onChange({
            campaign_id: undefined,
            connection_id: undefined,
            person_id: undefined,
            state: undefined,
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
