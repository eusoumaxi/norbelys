import { WebhookIcon } from "@hugeicons/core-free-icons";
import type { DeliveryObject, EventObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { ListTable } from "@/components/data-table";
import { DetailList } from "@/components/details";
import { DialogActions } from "@/components/dialog-actions";
import { JsonView } from "@/components/json-view";
import { DialogPending } from "@/components/problem";
import { StatusBadge } from "@/components/status-badge";
import { When } from "@/components/time";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { eventQuery } from "@/features/events/queries";
import { AttemptStatus } from "@/features/webhooks/badges";
import { describeEventType } from "@/features/webhooks/event-types";
import {
  deliveryListQuery,
  endpointDirectoryQuery,
} from "@/features/webhooks/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { formatCount, shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** The endpoints this event was delivered to; a row opens that delivery on its endpoint's page. */
const EventDeliveries = ({ eventId }: { eventId: string }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const endpoints = useQuery(endpointDirectoryQuery(workspace));
  const urlOf = (id: string) =>
    endpoints.data?.data.find((endpoint) => endpoint.id === id)?.url ??
    shortId(id);
  return (
    <ListTable<DeliveryObject>
      columns={[
        {
          render: (d) => (
            <span className="text-fg block max-w-[280px] truncate font-mono text-xs">
              {urlOf(d.webhook_endpoint_id)}
            </span>
          ),
          header: "Endpoint",
          id: "endpoint",
        },
        {
          render: (d) => <StatusBadge kind="delivery" value={d.state} />,
          header: "State",
          id: "state",
        },
        {
          render: (d) => formatCount(d.attempts),
          header: "Attempts",
          id: "attempts",
        },
        {
          render: (d) => <AttemptStatus attempt={d.last_attempt} />,
          header: "Last answer",
          id: "answer",
        },
      ]}
      empty={{
        description:
          "No endpoint was subscribed to this type when it happened, or its deliveries are still being created.",
        icon: WebhookIcon,
        title: "Not delivered",
      }}
      onRowClick={(d) => {
        void navigate({
          params: { endpointId: d.webhook_endpoint_id, slug: workspace.slug },
          search: { delivery: d.id },
          to: "/w/$slug/webhooks/$endpointId",
        });
      }}
      query={deliveryListQuery(workspace, { event_id: eventId })}
      rowKey={(d) => d.id}
    />
  );
};

/** One event: when and why it was made, the data consumers receive, and where it went. */
const EventView = ({ event }: { event: EventObject }) => {
  const workspace = useWorkspace();
  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex flex-wrap items-center gap-2 font-mono text-xl">
          {event.type}
          {event.synthetic ? <Badge tone="beta">Test</Badge> : null}
        </DialogTitle>
        <DialogDescription>
          {describeEventType(event.type) ?? "An event of a newer type."}
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="gap-5">
        <DetailList
          rows={[
            { label: "ID", value: <Copyable mono value={event.id} /> },
            {
              label: "Created",
              value: <When value={event.created_at} />,
            },
            {
              label: "Origin",
              value: event.synthetic
                ? "Created on request (a test), with sample data"
                : "A real change in the workspace",
            },
            event.webhook_endpoint_id
              ? {
                  label: "Addressed to",
                  value: (
                    <Link
                      className="font-mono text-xs"
                      params={{
                        endpointId: event.webhook_endpoint_id,
                        slug: workspace.slug,
                      }}
                      to="/w/$slug/webhooks/$endpointId"
                    >
                      {event.webhook_endpoint_id}
                    </Link>
                  ),
                }
              : null,
          ]}
        />
        <section className="flex flex-col gap-2">
          <h3 className="text-fg text-base font-medium">Data</h3>
          <JsonView className="max-h-80 overflow-y-auto" value={event.data} />
        </section>
        <section className="flex flex-col gap-2">
          <h3 className="text-fg text-base font-medium">Deliveries</h3>
          <EventDeliveries eventId={event.id} />
        </section>
      </DialogBody>
      <DialogActions />
    </>
  );
};

/** Loads the event the address names, with a spinner and the problem when it fails. */
const EventContent = ({ id }: { id: string }) => {
  const workspace = useWorkspace();
  const event = useQuery(eventQuery(workspace, id));
  if (event.isSuccess) {
    return <EventView event={event.data} />;
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Event</DialogTitle>
      </DialogHeader>
      <DialogPending query={event} />
    </>
  );
};

/**
 * One event, opened by `?event=evt_…`: its data as consumers receive it, as JSON, and its
 * deliveries to the workspace's endpoints, each opening on its endpoint's page.
 */
export const EventDialog = () => {
  const dialog = useUrlDialog("event");
  return (
    <Dialog {...dialog.props}>
      <DialogContent className="max-w-[760px]">
        {dialog.value ? (
          <EventContent id={dialog.value} key={dialog.value} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
};
