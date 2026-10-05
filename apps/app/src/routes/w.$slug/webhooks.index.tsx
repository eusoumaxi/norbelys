import { Add01Icon, WebhookIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { EndpointObject } from "@norbelys/sdk";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import {
  createStandardSchemaV1,
  parseAsStringLiteral,
  useQueryState,
} from "nuqs";
import { useState } from "react";

import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { RelativeTime } from "@/components/time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Segmented } from "@/components/ui/segmented";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { EndpointStatusBadge } from "@/features/webhooks/badges";
import { EndpointCreateDialog } from "@/features/webhooks/endpoint-create-dialog";
import { DeleteEndpointDialog } from "@/features/webhooks/endpoint-settings";
import { endpointListQuery, endpointsKey } from "@/features/webhooks/queries";
import { useAction } from "@/lib/actions";
import { plural } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

const FILTERS = ["all", "enabled", "disabled"] as const;

const FILTER_OPTIONS = [
  { label: "All", value: "all" as const },
  { label: "Enabled", value: "enabled" as const },
  { label: "Disabled", value: "disabled" as const },
];

// Declared once for nuqs (state) and the router (typed links).
const search = {
  status: parseAsStringLiteral(FILTERS).withDefault("all"),
};

/** The event types of an endpoint: how many, with the list on hover. */
const EventCount = ({ types }: { types: string[] }) => (
  <Tooltip>
    <TooltipTrigger render={<span />}>
      <Badge>{plural(types.length, "event")}</Badge>
    </TooltipTrigger>
    <TooltipContent>
      <ul className="font-mono">
        {types.map((type) => (
          <li key={type}>{type}</li>
        ))}
      </ul>
    </TooltipContent>
  </Tooltip>
);

const EmptyAction = ({ onClick }: { onClick: () => void }) => (
  <Button onClick={onClick} variant="primary">
    <HugeiconsIcon icon={Add01Icon} />
    Add endpoint
  </Button>
);

/**
 * The workspace's webhook endpoints: where its events are POSTed, signed per Standard Webhooks.
 * A row opens the endpoint (its deliveries and settings); the menu enables, disables or deletes
 * it without leaving the list.
 */
const Webhooks = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const act = useAction();
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<EndpointObject | null>(null);
  const [status, setStatus] = useQueryState("status", search.status);
  const enabled = status === "all" ? undefined : status === "enabled";

  return (
    <PageBody>
      <PageHeader
        actions={<EmptyAction onClick={() => setCreating(true)} />}
        title="Webhooks"
      />
      <div className="flex flex-col gap-3">
        <Segmented
          label="Endpoint status"
          onChange={(value) => {
            void setStatus(value === "all" ? null : value);
          }}
          options={FILTER_OPTIONS}
          value={status}
        />
        <ListTable<EndpointObject>
          columns={[
            {
              render: (e) => (
                <span className="text-fg block max-w-[460px] truncate font-mono text-xs font-medium">
                  {e.url}
                </span>
              ),
              header: "URL",
              id: "url",
            },
            {
              render: (e) => <EndpointStatusBadge endpoint={e} />,
              header: "Status",
              id: "status",
            },
            {
              render: (e) => <EventCount types={e.event_types} />,
              header: "Events",
              id: "events",
            },
            {
              render: (e) => <RelativeTime value={e.updated_at} />,
              header: "Updated",
              id: "updated",
            },
            {
              render: (e) => (
                <RowMenu>
                  <DropdownMenuItem
                    onClick={() =>
                      act(
                        e.enabled ? "Endpoint disabled" : "Endpoint enabled",
                        () =>
                          workspace.api.webhookEndpoints.update(e.id, {
                            enabled: !e.enabled,
                          }),
                        endpointsKey(workspace)
                      )
                    }
                  >
                    {e.enabled ? "Disable" : "Enable"}
                  </DropdownMenuItem>
                  <CopyIdItem id={e.id} noun="endpoint" />
                  <DropdownMenuItem
                    className="text-error-fg"
                    onClick={() => setDeleting(e)}
                  >
                    Delete
                  </DropdownMenuItem>
                </RowMenu>
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            action:
              status === "all" ? (
                <EmptyAction onClick={() => setCreating(true)} />
              ) : undefined,
            description:
              status === "all"
                ? "Receive sends, failures, bounces, replies and health changes as signed HTTP POSTs to your own URL."
                : `No endpoint is ${status}.`,
            icon: WebhookIcon,
            illustration: status === "all" ? "webhook" : undefined,
            title: status === "all" ? "No webhook endpoints" : "No endpoints",
          }}
          onRowClick={(e) => {
            void navigate({
              params: { endpointId: e.id, slug: workspace.slug },
              to: "/w/$slug/webhooks/$endpointId",
            });
          }}
          query={endpointListQuery(workspace, enabled)}
          rowKey={(e) => e.id}
        />
      </div>
      <EndpointCreateDialog onOpenChange={setCreating} open={creating} />
      <DeleteEndpointDialog
        endpoint={deleting}
        onOpenChange={(open) => {
          if (!open) {
            setDeleting(null);
          }
        }}
        open={deleting !== null}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/webhooks/")({
  validateSearch: createStandardSchemaV1(search, { partialOutput: true }),
  head: () => ({ meta: [{ title: "Webhooks · Norbelys" }] }),
  component: Webhooks,
});
