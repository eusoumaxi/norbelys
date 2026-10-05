import type { EndpointObject, LastAttempt } from "@norbelys/sdk";

import { Dash } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import type { Tone } from "@/components/ui/badge";

/** The states a list of deliveries can be narrowed to, as the API names them. */
export const DELIVERY_STATES = [
  "pending",
  "delivered",
  "failed",
  "disabled",
] as const;

const toneOfStatus = (status: number): Tone => {
  if (status >= 200 && status < 300) {
    return "success";
  }
  if (status >= 500 || status === 429) {
    return "warning";
  }
  return "error";
};

/**
 * What the endpoint answered on a delivery's last attempt: its HTTP status in mono, and how long
 * it took; "No answer" when the request never got one (a timeout, a refused connection).
 */
export const AttemptStatus = ({
  attempt,
}: {
  attempt: LastAttempt | null | undefined;
}) => {
  if (!attempt) {
    return <Dash />;
  }
  const duration =
    attempt.duration_ms === null || attempt.duration_ms === undefined
      ? null
      : `${attempt.duration_ms} ms`;
  return (
    <span className="flex items-center gap-2">
      {attempt.response_status === null ||
      attempt.response_status === undefined ? (
        <Badge tone="error">No answer</Badge>
      ) : (
        <Badge
          className="font-mono"
          tone={toneOfStatus(attempt.response_status)}
        >
          {attempt.response_status}
        </Badge>
      )}
      {duration ? (
        <span className="text-fg-3 text-xs tabular-nums">{duration}</span>
      ) : null}
    </span>
  );
};

/**
 * An endpoint's state as one word: `failing` while enabled with a failure since its last
 * success, otherwise `enabled` or `disabled`.
 */
const endpointState = (endpoint: EndpointObject): string => {
  if (!endpoint.enabled) {
    return "disabled";
  }
  return endpoint.failing_since ? "failing" : "enabled";
};

/** An endpoint's state in a pill: enabled, failing (still retried) or disabled. */
export const EndpointStatusBadge = ({
  endpoint,
}: {
  endpoint: EndpointObject;
}) => <StatusBadge kind="endpoint" value={endpointState(endpoint)} />;
