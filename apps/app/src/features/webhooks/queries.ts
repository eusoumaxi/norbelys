import type {
  DeliveryObject,
  EndpointObject,
  WebhookDeliveryState,
} from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";

import { listQuery } from "@/components/data-table";
import type { ListQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** Every query about the workspace's endpoints starts with this key. */
export const endpointsKey = (workspace: Workspace) =>
  [workspace.id, "webhook_endpoints"] as const;

/** Every query about the workspace's webhook deliveries starts with this key. */
export const deliveriesKey = (workspace: Workspace) =>
  [workspace.id, "webhook_deliveries"] as const;

/** The workspace's endpoints, newest first; `enabled` keeps only enabled or disabled ones. */
export const endpointListQuery = (
  workspace: Workspace,
  enabled: boolean | undefined
): ListQuery<EndpointObject> =>
  listQuery(
    [...endpointsKey(workspace), "list", enabled ?? "all"],
    (cursor, signal) =>
      workspace.api.webhookEndpoints.list(
        { cursor, enabled, limit: 50 },
        { signal }
      )
  );

/**
 * The first hundred endpoints, to name an endpoint by its URL where only its id is known (a
 * delivery), to offer endpoints in a choice, and for the overview's count.
 */
export const endpointDirectoryQuery = (workspace: Workspace) =>
  queryOptions({
    queryKey: [...endpointsKey(workspace), "directory"],
    queryFn: async ({ signal }) =>
      await workspace.api.webhookEndpoints.list({ limit: 100 }, { signal }),
  });

/** One endpoint, without its secret. */
export const endpointQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...endpointsKey(workspace), "detail", id],
    queryFn: ({ signal }) =>
      workspace.api.webhookEndpoints.retrieve(id, { signal }),
  });

/** What a list of deliveries is narrowed to. */
interface DeliveryFilters {
  webhook_endpoint_id?: string;
  event_id?: string;
  state?: WebhookDeliveryState;
}

/**
 * Deliveries, newest events first, read again every 10 seconds while the list is shown: a
 * pending delivery's next attempt, or a test event just sent, lands without a reload.
 */
export const deliveryListQuery = (
  workspace: Workspace,
  filters: DeliveryFilters
): ListQuery<DeliveryObject> => ({
  ...listQuery(
    [...deliveriesKey(workspace), "list", filters],
    (cursor, signal) =>
      workspace.api.webhookDeliveries.list(
        { cursor, limit: 50, ...filters },
        { signal }
      )
  ),
  refetchInterval: 10_000,
});

/** One delivery with its latest attempt. */
export const deliveryQuery = (workspace: Workspace, id: string) =>
  queryOptions({
    queryKey: [...deliveriesKey(workspace), "detail", id],
    queryFn: ({ signal }) =>
      workspace.api.webhookDeliveries.retrieve(id, { signal }),
  });
