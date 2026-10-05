import type { EventType } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { eventsKey } from "@/features/events/queries";
import {
  describeEventType,
  EVENT_TYPES,
} from "@/features/webhooks/event-types";
import {
  deliveriesKey,
  endpointDirectoryQuery,
} from "@/features/webhooks/queries";
import { FormField } from "@/lib/form";
import { problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

const TYPE_OPTIONS = EVENT_TYPES.map((type) => ({ label: type, value: type }));

/** The value of the endpoint choice that sends to every subscribed endpoint. */
const EVERY_ENDPOINT = "all";

/** How long after sending the lists are read again: deliveries appear within seconds. */
const SETTLE_MS = 2500;

/** The endpoint choice: one endpoint, or every endpoint subscribed to the type. */
const EndpointChoice = ({
  onChange,
  value,
}: {
  onChange: (value: string) => void;
  value: string;
}) => {
  const workspace = useWorkspace();
  const endpoints = useQuery(endpointDirectoryQuery(workspace));
  return (
    <FormField
      description="One endpoint receives the event whatever it subscribes to."
      htmlFor="test-event-endpoint"
      label="Send to"
    >
      <Select
        id="test-event-endpoint"
        onChange={onChange}
        options={[
          {
            label: "Every endpoint subscribed to the type",
            value: EVERY_ENDPOINT,
          },
          ...(endpoints.data?.data ?? []).map((endpoint) => ({
            label: endpoint.url,
            value: endpoint.id,
          })),
        ]}
        value={value}
      />
    </FormField>
  );
};

/**
 * Sends a synthetic event: sample data of the chosen type, delivered, signed and retried like
 * any real one, and marked `synthetic`. With `endpointId` it goes to that endpoint alone,
 * whatever its subscriptions (how an endpoint is tested); without it, the person picks one
 * endpoint or every endpoint subscribed to the type. `onSent` gets the new event's id once the
 * API took it.
 */
export const TestEventDialog = ({
  endpointId,
  onOpenChange,
  onSent,
  open,
}: {
  endpointId?: string;
  onOpenChange: (open: boolean) => void;
  onSent?: (eventId: string) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [type, setType] = useState<EventType>("endpoint.test");
  const [target, setTarget] = useState(EVERY_ENDPOINT);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const send = async () => {
    setBusy(true);
    setFailure(null);
    const chosen = target === EVERY_ENDPOINT ? undefined : target;
    try {
      const event = await workspace.api.events.create({
        type,
        webhook_endpoint_id: endpointId ?? chosen,
      });
      toast.success(`Test event ${event.id} sent`, {
        description: "Its deliveries appear within seconds.",
      });
      setTimeout(() => {
        void queryClient.invalidateQueries({
          queryKey: deliveriesKey(workspace),
        });
        void queryClient.invalidateQueries({ queryKey: eventsKey(workspace) });
      }, SETTLE_MS);
      onOpenChange(false);
      onSent?.(event.id);
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };
  return (
    <Dialog
      onOpenChange={(next) => {
        onOpenChange(next);
        if (!next) {
          setFailure(null);
        }
      }}
      open={open}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Send a test event</DialogTitle>
          <DialogDescription>
            Sample data of the type you choose, delivered and signed like a real
            event, and marked synthetic so your code can tell it apart.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <FormField
            description={describeEventType(type)}
            htmlFor="test-event-type"
            label="Event type"
          >
            <Select
              id="test-event-type"
              onChange={(value) => {
                const known = EVENT_TYPES.find((option) => option === value);
                if (known) {
                  setType(known);
                }
              }}
              options={TYPE_OPTIONS}
              value={type}
            />
          </FormField>
          {endpointId ? null : (
            <EndpointChoice onChange={setTarget} value={target} />
          )}
          {failure ? <ProblemAlert>{problemLine(failure)}</ProblemAlert> : null}
        </DialogBody>
        <DialogActions>
          <Button
            disabled={busy}
            onClick={() => {
              void send();
            }}
            variant="primary"
          >
            {busy ? <Spinner /> : null}
            Send test event
          </Button>
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
};
