import type {
  ClassificationSource,
  InboundReview,
  ThreadObject,
} from "@norbelys/sdk";

import { Dash } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import type { SelectOption } from "@/components/ui/select";
import { CLASSIFICATIONS } from "@/features/inbox/queries";
import { statusLabel } from "@/lib/status";

/** The classifications as choices, with "all" first when the choice is a filter. */
export const classificationOptions = (all?: {
  label: string;
  value: string;
}): SelectOption[] => [
  ...(all ? [all] : []),
  ...CLASSIFICATIONS.map((value) => ({
    label: statusLabel("classification", value),
    value,
  })),
];

/** Who decided a classification. */
export const SOURCES: Record<ClassificationSource, string> = {
  ai: "AI",
  manual: "Corrected by hand",
  rules: "Rules",
};

/**
 * Where a review stands: waiting for a person, or decided; nothing when no review was asked for.
 */
export const ReviewBadge = ({ review }: { review: InboundReview }) => {
  if (review.decision) {
    return (
      <Badge tone={review.decision === "confirmed" ? "success" : "muted"}>
        {review.decision === "confirmed" ? "Confirmed" : "Dismissed"}
      </Badge>
    );
  }
  if (review.requested_at) {
    return (
      <Badge dot tone="warning">
        Needs review
      </Badge>
    );
  }
  return null;
};

/** How a thread's last message reads: its classification when it is inbound, else its direction. */
export const LastMessage = ({ thread }: { thread: ThreadObject }) => {
  const last = thread.last_message;
  if (!last) {
    return <Dash />;
  }
  if (last.classification) {
    return <StatusBadge kind="classification" value={last.classification} />;
  }
  return (
    <span className="text-fg-3">
      {last.direction === "outbound" ? "Sent" : "Received"}
    </span>
  );
};
