import {
  PauseIcon,
  PlayIcon,
  RepeatIcon,
  SentIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { EndpointObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { CreateDialog } from "@/components/create-dialog";
import { Button } from "@/components/ui/button";
import {
  deliveriesKey,
  endpointQuery,
  endpointsKey,
} from "@/features/webhooks/queries";
import { TestEventDialog } from "@/features/webhooks/test-event-dialog";
import { useAction } from "@/lib/actions";
import { DAY_MS, toLocalInput } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/**
 * Where a replay starts unless the person says otherwise: the failure's start when the endpoint
 * is failing, else a day ago. Read when the dialog opens, never while rendering.
 */
const defaultSince = (endpoint: EndpointObject): string =>
  toLocalInput(
    endpoint.failing_since
      ? new Date(endpoint.failing_since)
      : new Date(Date.now() - DAY_MS)
  );

/**
 * Reopens the endpoint's deliveries since an instant: each becomes pending with an attempt now,
 * delivered ones included, so the receiver must deduplicate on `webhook-id`. `since` is where
 * the instant starts, as a `datetime-local` value.
 */
const ReplayDialog = ({
  endpoint,
  onClose,
  since,
}: {
  endpoint: EndpointObject;
  onClose: () => void;
  since: string;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  return (
    <CreateDialog
      description="Every delivery of its events created since then is attempted again now, delivered ones too: deduplicate on webhook-id. Events older than the replay window cannot be replayed."
      fields={[
        {
          description: "In your browser's time zone.",
          initial: since,
          label: "Since",
          name: "since",
          required: true,
          type: "datetime-local",
        },
      ]}
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
      onSubmit={async (values) => {
        await workspace.api.webhookEndpoints.replay(endpoint.id, {
          since: new Date(values.since ?? "").toISOString(),
        });
        toast.success("Deliveries reopened: attempts run now");
        void queryClient.invalidateQueries({
          queryKey: deliveriesKey(workspace),
        });
        onClose();
      }}
      open
      submitLabel="Replay"
      title="Replay deliveries"
    />
  );
};

/**
 * The endpoint page's header actions: send a test event to it, replay its deliveries since an
 * instant (only while enabled, as the API requires), and enable or disable it. Disabling stops
 * its pending deliveries; enabling clears its failure record.
 */
export const EndpointActions = ({ endpoint }: { endpoint: EndpointObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const act = useAction();
  const [testing, setTesting] = useState(false);
  const [replayFrom, setReplayFrom] = useState<string | null>(null);
  const toggle = async () => {
    const updated = await workspace.api.webhookEndpoints.update(endpoint.id, {
      enabled: !endpoint.enabled,
    });
    queryClient.setQueryData(
      endpointQuery(workspace, endpoint.id).queryKey,
      updated
    );
  };
  return (
    <>
      <Button onClick={() => setTesting(true)} variant="secondary">
        <HugeiconsIcon icon={SentIcon} />
        Send test event
      </Button>
      <Button
        disabled={!endpoint.enabled}
        onClick={() => setReplayFrom(defaultSince(endpoint))}
        title={
          endpoint.enabled ? undefined : "Enable the endpoint before replaying"
        }
        variant="secondary"
      >
        <HugeiconsIcon icon={RepeatIcon} />
        Replay
      </Button>
      <Button
        onClick={() =>
          act(
            endpoint.enabled ? "Endpoint disabled" : "Endpoint enabled",
            toggle,
            endpointsKey(workspace)
          )
        }
        variant="secondary"
      >
        <HugeiconsIcon icon={endpoint.enabled ? PauseIcon : PlayIcon} />
        {endpoint.enabled ? "Disable" : "Enable"}
      </Button>
      <TestEventDialog
        endpointId={endpoint.id}
        onOpenChange={setTesting}
        onSent={() => {
          // Its delivery shows on the deliveries tab, which reads itself again within seconds.
          void navigate({
            params: { endpointId: endpoint.id, slug: workspace.slug },
            to: "/w/$slug/webhooks/$endpointId",
          });
        }}
        open={testing}
      />
      {replayFrom ? (
        <ReplayDialog
          endpoint={endpoint}
          onClose={() => setReplayFrom(null)}
          since={replayFrom}
        />
      ) : null}
    </>
  );
};
