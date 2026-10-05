import {
  Activity01Icon,
  Refresh01Icon,
  SentIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { EventObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { createStandardSchemaV1, parseAsString, useQueryState } from "nuqs";
import { useState } from "react";

import { CodeLine } from "@/components/copy";
import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { RelativeTime } from "@/components/time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { EventDialog } from "@/features/events/event-dialog";
import { eventListQuery, eventsKey } from "@/features/events/queries";
import {
  EVENT_TYPE_OPTIONS,
  isEventType,
} from "@/features/webhooks/event-types";
import { TestEventDialog } from "@/features/webhooks/test-event-dialog";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { shortId } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

// Declared once for nuqs (state) and the router (typed links).
const search = {
  event: parseAsString,
  type: parseAsString,
};

/** Where an event came from: a real change, or a test made on request (and for whom). */
const Origin = ({ event }: { event: EventObject }) => {
  if (!event.synthetic) {
    return <span className="text-fg-3">Workspace</span>;
  }
  return (
    <span className="flex items-center gap-2">
      <Badge tone="beta">Test</Badge>
      {event.webhook_endpoint_id ? (
        <span className="text-fg-3 text-xs">to one endpoint</span>
      ) : null}
    </span>
  );
};

/**
 * The workspace's events, newest first: every change a webhook endpoint can receive, filtered by
 * type. A row opens the event's data and its deliveries; a test event can be sent from here, and
 * the CLI forwards the stream to a local server.
 */
const EventsPage = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const dialog = useUrlDialog("event");
  const [testing, setTesting] = useState(false);
  const [type, setType] = useQueryState("type", search.type);
  const known = type && isEventType(type) ? type : undefined;
  return (
    <PageBody>
      <PageHeader
        actions={
          <>
            <Button
              onClick={() => {
                void queryClient.invalidateQueries({
                  queryKey: eventsKey(workspace),
                });
              }}
              variant="secondary"
            >
              <HugeiconsIcon icon={Refresh01Icon} />
              Refresh
            </Button>
            <Button onClick={() => setTesting(true)} variant="primary">
              <HugeiconsIcon icon={SentIcon} />
              Send test event
            </Button>
          </>
        }
        title="Events"
      />
      <div className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <Select
            className="w-72"
            label="Event type"
            onChange={(value) => {
              void setType(value === "all" ? null : value);
            }}
            options={EVENT_TYPE_OPTIONS}
            value={known ?? "all"}
          />
          <div className="flex min-w-0 items-center gap-3">
            <span className="text-fg-2 hidden text-xs md:inline">
              Forward them to your machine
            </span>
            <CodeLine
              className="w-[380px] max-w-full"
              value="norbelys listen --forward-to localhost:3000/hooks"
            />
          </div>
        </div>
        <ListTable<EventObject>
          columns={[
            {
              render: (e) => (
                <code className="text-fg font-mono text-xs font-medium">
                  {e.type}
                </code>
              ),
              header: "Type",
              id: "type",
            },
            {
              render: (e) => (
                <span className="text-fg-3 font-mono text-xs" title={e.id}>
                  {shortId(e.id)}
                </span>
              ),
              header: "ID",
              id: "id",
            },
            {
              render: (e) => <Origin event={e} />,
              header: "Origin",
              id: "origin",
            },
            {
              render: (e) => <RelativeTime value={e.created_at} />,
              header: "Created",
              id: "created",
            },
          ]}
          empty={{
            description: known
              ? `No ${known} event yet.`
              : "Events appear as messages are sent, replies arrive and campaigns change. Send a test event to see one now.",
            icon: Activity01Icon,
            title: "No events",
          }}
          onRowClick={(e) => dialog.open(e.id)}
          query={eventListQuery(workspace, known)}
          rowKey={(e) => e.id}
        />
      </div>
      <EventDialog />
      <TestEventDialog
        onOpenChange={setTesting}
        onSent={(id) => dialog.open(id)}
        open={testing}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/events")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Events · Norbelys" }] }),
  component: EventsPage,
});
