import type { DeliveryEventKind, DeliveryEventObject } from "@norbelys/sdk";

import { listQuery } from "@/components/data-table";
import type { ListQuery } from "@/components/data-table";
import type { Workspace } from "@/lib/workspace";

/** What the delivery logs are narrowed to: every filter the API accepts. */
interface LogFilters {
  campaign_id?: string;
  kind?: DeliveryEventKind;
  message_id?: string;
  person_id?: string;
  recipient?: string;
}

/** Every query about the workspace's delivery events starts with this key. */
export const deliveryEventsKey = (workspace: Workspace) =>
  [workspace.id, "delivery_events"] as const;

/** The workspace's delivery events, newest first, narrowed by `filters`. */
export const deliveryEventListQuery = (
  workspace: Workspace,
  filters: LogFilters
): ListQuery<DeliveryEventObject> =>
  listQuery(
    [...deliveryEventsKey(workspace), "list", filters],
    (cursor, signal) =>
      workspace.api.deliveryEvents.list(
        { cursor, limit: 50, ...filters },
        { signal }
      )
  );
