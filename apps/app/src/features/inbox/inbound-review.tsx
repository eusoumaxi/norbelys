import type { InboundMessageObject, ReviewProposal } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { useAction } from "@/lib/actions";
import { formatTimestamp, humanize } from "@/lib/format";
import { statusLabel } from "@/lib/status";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** What a proposal would do, in a sentence. */
const describeProposal = (
  proposal: ReviewProposal | null | undefined
): string => {
  if (!proposal) {
    return "Nothing beyond recording the decision.";
  }
  if (proposal.action === "suppress") {
    return `Suppress ${proposal.email} (${humanize(proposal.reason).toLowerCase()}): nothing is sent to it again, and the live enrollments of whoever has it end.`;
  }
  if (proposal.action === "change_address") {
    return proposal.new_email
      ? `Move the person at ${proposal.email} to ${proposal.new_email}, and suppress the old address as changed; their enrollments go on at the new one.`
      : `Suppress ${proposal.email} as changed (the notice gave no new address), ending its live enrollments.`;
  }
  return `Classify it as ${statusLabel("classification", proposal.classification).toLowerCase()}, ${proposal.sentiment} sentiment, as the AI judged it at ${proposal.confidence_percent}% confidence. A classification corrected by hand stays as it is.`;
};

/**
 * What a rule or the classifier asked a person to decide about this message: what confirming
 * applies, with Confirm and Dismiss while it waits, or the decision once made. Nothing a person
 * wrote is applied without this decision. Nothing shows when no review was asked for.
 */
export const ReviewCard = ({ message }: { message: InboundMessageObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const act = useAction();
  const { review } = message;
  if (!review.requested_at) {
    return null;
  }
  if (review.decision) {
    return (
      <Alert variant="neutral">
        <AlertTitle>
          {review.decision === "confirmed" ? "Confirmed" : "Dismissed"}
          {review.reviewed_at
            ? ` on ${formatTimestamp(review.reviewed_at)}`
            : ""}
        </AlertTitle>
        <AlertDescription>
          It proposed: {describeProposal(review.proposal)}
        </AlertDescription>
      </Alert>
    );
  }
  // A decision can suppress an address or move a person: refresh everything it may change.
  const decide = (decision: "confirm" | "dismiss") =>
    act(
      decision === "confirm" ? "Review confirmed" : "Review dismissed",
      () => workspace.api.inboundMessages.review(message.id, { decision }),
      () => queryClient.invalidateQueries({ queryKey: [workspace.id] })
    );
  return (
    <Alert variant="warning">
      <AlertTitle>Needs your review</AlertTitle>
      <AlertDescription className="flex flex-col gap-3">
        <span>Confirming applies it: {describeProposal(review.proposal)}</span>
        {canWrite(workspace) ? (
          <span className="flex flex-wrap gap-2">
            <Button
              onClick={() => decide("confirm")}
              size="s"
              variant="primary"
            >
              Confirm
            </Button>
            <Button
              onClick={() => decide("dismiss")}
              size="s"
              variant="secondary"
            >
              Dismiss
            </Button>
          </span>
        ) : null}
      </AlertDescription>
    </Alert>
  );
};
