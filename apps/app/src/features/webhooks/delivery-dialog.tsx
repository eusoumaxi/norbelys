import type { DeliveryObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";

import { CodeBlock, Copyable } from "@/components/copy";
import { DetailList } from "@/components/details";
import { DialogActions } from "@/components/dialog-actions";
import { DialogPending } from "@/components/problem";
import { StatusBadge } from "@/components/status-badge";
import { When } from "@/components/time";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { AttemptStatus } from "@/features/webhooks/badges";
import { deliveriesKey, deliveryQuery } from "@/features/webhooks/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { useAction } from "@/lib/actions";
import { formatCount, formatRelative } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** What the endpoint answered last: the status, the time and the first kilobyte of the body. */
const LastAttemptDetails = ({ delivery }: { delivery: DeliveryObject }) => {
  const attempt = delivery.last_attempt;
  if (!attempt) {
    return (
      <p className="text-fg-3 text-sm">
        No attempt yet: the first one is due{" "}
        {delivery.next_attempt_at
          ? formatRelative(delivery.next_attempt_at).toLowerCase()
          : "shortly"}
        .
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-3">
      <DetailList
        rows={[
          { label: "Started", value: <When value={attempt.started_at} /> },
          { label: "Answer", value: <AttemptStatus attempt={attempt} /> },
        ]}
      />
      {attempt.response_excerpt ? (
        <CodeBlock
          className="max-h-60 overflow-y-auto"
          value={attempt.response_excerpt}
        />
      ) : (
        <p className="text-fg-3 text-xs">The answer had no body.</p>
      )}
    </div>
  );
};

/** One delivery's details and its retry. */
const DeliveryView = ({ delivery }: { delivery: DeliveryObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  const [busy, setBusy] = useState(false);
  const retry = async () => {
    setBusy(true);
    await action(
      "Retry scheduled: an attempt runs now",
      async () => {
        const updated = await workspace.api.webhookDeliveries.retry(
          delivery.id
        );
        queryClient.setQueryData(
          deliveryQuery(workspace, delivery.id).queryKey,
          updated
        );
      },
      () => {
        void queryClient.invalidateQueries({
          queryKey: [...deliveriesKey(workspace), "list"],
        });
      }
    );
    setBusy(false);
  };
  return (
    <>
      <DialogHeader>
        <DialogTitle className="font-mono text-xl">
          {delivery.event_type}
        </DialogTitle>
        <DialogDescription render={<div />}>
          <Copyable mono value={delivery.id} />
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="gap-5">
        <DetailList
          rows={[
            {
              label: "State",
              value: <StatusBadge kind="delivery" value={delivery.state} />,
            },
            { label: "Attempts", value: formatCount(delivery.attempts) },
            delivery.next_attempt_at
              ? {
                  label: "Next attempt",
                  value: <When value={delivery.next_attempt_at} />,
                }
              : null,
            delivery.delivered_at
              ? {
                  label: "Delivered",
                  value: <When value={delivery.delivered_at} />,
                }
              : null,
            { label: "Created", value: <When value={delivery.created_at} /> },
            {
              label: "Event",
              value: (
                <Link
                  className="font-mono text-xs"
                  params={{ slug: workspace.slug }}
                  search={{ event: delivery.event_id }}
                  to="/w/$slug/events"
                >
                  {delivery.event_id}
                </Link>
              ),
            },
          ]}
        />
        <section className="flex flex-col gap-3">
          <h3 className="text-fg text-base font-medium">Last attempt</h3>
          <LastAttemptDetails delivery={delivery} />
        </section>
      </DialogBody>
      <DialogActions
        note={
          delivery.state === "pending"
            ? "Retrying brings the next attempt forward."
            : "Retrying makes it pending, with an attempt now."
        }
      >
        <Button
          disabled={busy}
          onClick={() => {
            void retry();
          }}
          variant="primary"
        >
          {busy ? <Spinner /> : null}
          Retry now
        </Button>
      </DialogActions>
    </>
  );
};

/** Loads the delivery the address names, with a spinner and the problem when it fails. */
const DeliveryContent = ({ id }: { id: string }) => {
  const workspace = useWorkspace();
  const delivery = useQuery(deliveryQuery(workspace, id));
  if (delivery.isSuccess) {
    return <DeliveryView delivery={delivery.data} />;
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Delivery</DialogTitle>
      </DialogHeader>
      <DialogPending query={delivery} />
    </>
  );
};

/**
 * One webhook delivery, opened by `?delivery=whd_…`: its state, attempts, next attempt, and what
 * the endpoint answered last (status, time, the first kilobyte), with "Retry now".
 */
export const DeliveryDialog = () => {
  const dialog = useUrlDialog("delivery");
  return (
    <Dialog {...dialog.props}>
      <DialogContent className="max-w-[640px]">
        {dialog.value ? (
          <DeliveryContent id={dialog.value} key={dialog.value} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
};
