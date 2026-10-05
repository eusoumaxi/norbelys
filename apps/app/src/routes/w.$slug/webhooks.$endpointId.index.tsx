import { WebhookIcon } from "@hugeicons/core-free-icons";
import type { DeliveryObject } from "@norbelys/sdk";
import { createFileRoute } from "@tanstack/react-router";
import {
  createStandardSchemaV1,
  parseAsString,
  parseAsStringLiteral,
  useQueryState,
} from "nuqs";

import { Dash, ListTable } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { Segmented } from "@/components/ui/segmented";
import { AttemptStatus, DELIVERY_STATES } from "@/features/webhooks/badges";
import { DeliveryDialog } from "@/features/webhooks/delivery-dialog";
import { deliveryListQuery } from "@/features/webhooks/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { formatCount, formatRelative, shortId } from "@/lib/format";
import { statusLabel } from "@/lib/status";
import { useWorkspace } from "@/lib/workspace";

const STATES = ["all", ...DELIVERY_STATES] as const;

const STATE_OPTIONS = STATES.map((value) => ({
  label: value === "all" ? "All" : statusLabel("delivery", value),
  value,
}));

// Declared once for nuqs (state) and the router (typed links).
const search = {
  delivery: parseAsString,
  state: parseAsStringLiteral(STATES).withDefault("all"),
};

/** When a delivery happened, or is due next: what a person scanning the list wants to know. */
const DeliveryTime = ({ delivery }: { delivery: DeliveryObject }) => {
  if (delivery.delivered_at) {
    return <>Delivered {formatRelative(delivery.delivered_at).toLowerCase()}</>;
  }
  if (delivery.state === "pending" && delivery.next_attempt_at) {
    return (
      <>Next attempt {formatRelative(delivery.next_attempt_at).toLowerCase()}</>
    );
  }
  if (delivery.last_attempt) {
    return (
      <>
        Tried {formatRelative(delivery.last_attempt.started_at).toLowerCase()}
      </>
    );
  }
  return <Dash />;
};

/**
 * The endpoint's deliveries, newest events first, narrowed by state and read again every 10
 * seconds. A row opens the delivery: its attempts, the endpoint's last answer, and a retry.
 */
const EndpointDeliveries = () => {
  const workspace = useWorkspace();
  const { endpointId } = Route.useParams();
  const dialog = useUrlDialog("delivery");
  const [state, setState] = useQueryState("state", search.state);
  return (
    <div className="flex flex-col gap-3">
      <Segmented
        label="Delivery state"
        onChange={(value) => {
          void setState(value === "all" ? null : value);
        }}
        options={STATE_OPTIONS}
        value={state}
      />
      <ListTable<DeliveryObject>
        columns={[
          {
            render: (d) => (
              <span className="flex min-w-0 flex-col">
                <code className="text-fg truncate font-mono text-xs font-medium">
                  {d.event_type}
                </code>
                <span className="text-fg-3 font-mono text-xs">
                  {shortId(d.event_id)}
                </span>
              </span>
            ),
            header: "Event",
            id: "event",
          },
          {
            render: (d) => <StatusBadge kind="delivery" value={d.state} />,
            header: "State",
            id: "state",
          },
          {
            render: (d) => (
              <span className="tabular-nums">{formatCount(d.attempts)}</span>
            ),
            header: "Attempts",
            id: "attempts",
          },
          {
            render: (d) => <AttemptStatus attempt={d.last_attempt} />,
            header: "Last answer",
            id: "answer",
          },
          {
            render: (d) => (
              <span className="text-fg-2">
                <DeliveryTime delivery={d} />
              </span>
            ),
            header: "When",
            id: "when",
          },
        ]}
        empty={{
          description:
            state === "all"
              ? "Each event this endpoint subscribes to is delivered here. Send a test event to see one arrive."
              : `No delivery is ${state}.`,
          icon: WebhookIcon,
          title: "No deliveries",
        }}
        onRowClick={(d) => dialog.open(d.id)}
        query={deliveryListQuery(workspace, {
          state: state === "all" ? undefined : state,
          webhook_endpoint_id: endpointId,
        })}
        rowKey={(d) => d.id}
      />
      <DeliveryDialog />
    </div>
  );
};

export const Route = createFileRoute("/w/$slug/webhooks/$endpointId/")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  component: EndpointDeliveries,
});
