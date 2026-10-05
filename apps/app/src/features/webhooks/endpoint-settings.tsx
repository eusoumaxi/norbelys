import type { EndpointObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { SettingsPanel } from "@/components/settings-layout";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { EndpointFields } from "@/features/webhooks/endpoint-create-dialog";
import { toSubscription } from "@/features/webhooks/event-types";
import {
  endpointQuery,
  endpointsKey,
  deliveriesKey,
} from "@/features/webhooks/queries";
import { SecretDialog } from "@/features/webhooks/secret-dialog";
import { useAction } from "@/lib/actions";
import { describeProblem } from "@/lib/problem";
import type { Workspace } from "@/lib/workspace";
import { useWorkspace } from "@/lib/workspace";

const sameTypes = (a: readonly string[], b: readonly string[]): boolean =>
  a.length === b.length && a.every((type) => b.includes(type));

/** Whether two readings of an endpoint have the same URL and event types. */
const sameSettings = (a: EndpointObject, b: EndpointObject): boolean =>
  a.url === b.url && sameTypes(a.event_types, b.event_types);

/**
 * Saves the form's URL and types over `base`, the reading the form started from. The endpoint
 * is read again first: when its settings changed elsewhere, nothing is written and `null` comes
 * back; otherwise the update carries that fresh reading's version in `If-Match`. An endpoint's
 * version also moves with its health (a failed attempt moves it), which this adopts, so a
 * failing endpoint can still be edited while a real concurrent edit is never overwritten.
 */
const saveSettings = async (
  workspace: Workspace,
  queryClient: QueryClient,
  base: EndpointObject,
  changes: { url: string; event_types: string[] }
): Promise<EndpointObject | null> => {
  const current = await queryClient.fetchQuery({
    ...endpointQuery(workspace, base.id),
    staleTime: 0,
  });
  if (!sameSettings(current, base)) {
    return null;
  }
  return await workspace.api.webhookEndpoints.update(
    base.id,
    { event_types: toSubscription(changes.event_types), url: changes.url },
    { headers: { "If-Match": `"${current.version}"` } }
  );
};

/** The note that the endpoint's settings changed since the form was filled, with a reload. */
const Conflict = ({ onReload }: { onReload: () => void }) => (
  <Alert variant="warning">
    <AlertTitle>Changed elsewhere</AlertTitle>
    <AlertDescription className="flex flex-col items-start gap-3">
      Someone changed this endpoint&apos;s URL or events since you opened it.
      Nothing was saved. Load the current settings, then make your change again.
      <Button onClick={onReload} size="s" variant="secondary">
        Load current settings
      </Button>
    </AlertDescription>
  </Alert>
);

/**
 * The endpoint's URL and event types, saved together with `If-Match` so a change made elsewhere
 * is never overwritten. While nothing is edited, the form follows the endpoint as it is read
 * again; the API's validation shows beside each field.
 */
export const EndpointSettingsForm = ({
  endpoint,
}: {
  endpoint: EndpointObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [base, setBase] = useState(endpoint);
  const [url, setUrl] = useState(endpoint.url);
  const [types, setTypes] = useState<string[]>(endpoint.event_types);
  const [busy, setBusy] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const dirty = url.trim() !== base.url || !sameTypes(types, base.event_types);
  const load = (reading: EndpointObject) => {
    setBase(reading);
    setUrl(reading.url);
    setTypes(reading.event_types);
    setConflict(false);
    setFailure(null);
  };
  if (!dirty && !sameSettings(endpoint, base)) {
    load(endpoint);
  }
  const save = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const saved = await saveSettings(workspace, queryClient, base, {
        event_types: types,
        url: url.trim(),
      });
      if (saved) {
        queryClient.setQueryData(
          endpointQuery(workspace, saved.id).queryKey,
          saved
        );
        void queryClient.invalidateQueries({
          queryKey: [...endpointsKey(workspace), "list"],
        });
        load(saved);
        toast.success("Endpoint saved");
      } else {
        setConflict(true);
      }
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };
  return (
    <form
      className="flex flex-col gap-5"
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      <EndpointFields
        failure={failure}
        onTypesChange={setTypes}
        onUrlChange={setUrl}
        types={types}
        typesDescription="A type added later is not received until it is ticked here."
        url={url}
      />
      {conflict ? <Conflict onReload={() => load(endpoint)} /> : null}
      <div className="flex items-center gap-2">
        <Button
          disabled={busy || !dirty || !url.trim() || types.length === 0}
          type="submit"
          variant="primary"
        >
          {busy ? <Spinner /> : null}
          Save changes
        </Button>
        {dirty ? (
          <Button onClick={() => load(endpoint)} variant="tertiary">
            Discard
          </Button>
        ) : null}
      </div>
    </form>
  );
};

/**
 * Rotating the signing secret: the new one is shown once, and the old one keeps signing for 24
 * hours (every attempt carries both signatures meanwhile), so the receiver can switch at its
 * own pace.
 */
export const RotateSecretPanel = ({
  endpoint,
}: {
  endpoint: EndpointObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const rotate = async () => {
    try {
      const rotated = await workspace.api.webhookEndpoints.rotateSecret(
        endpoint.id
      );
      queryClient.setQueryData(endpointQuery(workspace, endpoint.id).queryKey, {
        ...rotated,
        secret: null,
      });
      setSecret(rotated.secret ?? null);
    } catch (error) {
      toast.error(describeProblem(error).detail);
    }
  };
  return (
    <SettingsPanel
      description="The secret is shown once, when it is made. After a rotation the old secret keeps signing for 24 hours, so every attempt carries both signatures while you deploy the new one."
      title="Signing secret"
    >
      <div>
        <Button onClick={() => setConfirming(true)} variant="secondary">
          Rotate secret
        </Button>
      </div>
      <ConfirmDialog
        confirmLabel="Rotate secret"
        description="A new secret is made and shown once. The current one keeps signing for 24 hours, then stops."
        onConfirm={rotate}
        onOpenChange={setConfirming}
        open={confirming}
        title="Rotate the signing secret?"
      />
      <SecretDialog
        onClose={() => setSecret(null)}
        secret={secret}
        title="Copy the new signing secret"
      />
    </SettingsPanel>
  );
};

/**
 * Asks before deleting an endpoint with its deliveries. `onDeleted` runs once the API agreed and
 * before the endpoint's queries are dropped: a page that shows the endpoint leaves first, since
 * forgetting or refreshing it there would read it again and fail on the deletion.
 */
export const DeleteEndpointDialog = ({
  endpoint,
  onDeleted,
  onOpenChange,
  open,
}: {
  endpoint: EndpointObject | null;
  onDeleted?: () => Promise<unknown>;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  return (
    <ConfirmDialog
      confirmLabel="Delete endpoint"
      danger
      description={
        <>
          Events stop being sent to{" "}
          <span className="text-fg font-mono">{endpoint?.url}</span>, and its
          delivery history is deleted. This can&apos;t be undone.
        </>
      }
      onConfirm={() =>
        endpoint
          ? action(
              "Endpoint deleted",
              () => workspace.api.webhookEndpoints.delete(endpoint.id),
              async () => {
                await onDeleted?.();
                queryClient.removeQueries({
                  queryKey: endpointQuery(workspace, endpoint.id).queryKey,
                });
                void queryClient.invalidateQueries({
                  queryKey: endpointsKey(workspace),
                });
                void queryClient.invalidateQueries({
                  queryKey: deliveriesKey(workspace),
                });
              }
            )
          : undefined
      }
      onOpenChange={onOpenChange}
      open={open}
      title="Delete this endpoint?"
    />
  );
};

/** Deleting the endpoint, with its deliveries, after a confirmation; then back to the list. */
export const DeleteEndpointPanel = ({
  endpoint,
}: {
  endpoint: EndpointObject;
}) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);
  return (
    <SettingsPanel
      description="Its deliveries are deleted with it; the workspace's events stay. Disable it instead to stop deliveries for a while."
      title="Delete endpoint"
    >
      <div>
        <Button onClick={() => setConfirming(true)} variant="danger-secondary">
          Delete endpoint
        </Button>
      </div>
      <DeleteEndpointDialog
        endpoint={endpoint}
        onDeleted={() =>
          navigate({
            params: { slug: workspace.slug },
            to: "/w/$slug/webhooks",
          })
        }
        onOpenChange={setConfirming}
        open={confirming}
      />
    </SettingsPanel>
  );
};
