import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";

import { SettingsPanel } from "@/components/settings-layout";
import {
  DeleteEndpointPanel,
  EndpointSettingsForm,
  RotateSecretPanel,
} from "@/features/webhooks/endpoint-settings";
import { endpointQuery } from "@/features/webhooks/queries";
import { VerifySignatures } from "@/features/webhooks/verify-signatures";
import { useWorkspace } from "@/lib/workspace";

/**
 * The endpoint's settings: its URL and event types, its signing secret (rotated, never read
 * again), how to verify its signatures, and its deletion.
 */
const EndpointSettings = () => {
  const workspace = useWorkspace();
  const { endpointId } = Route.useParams();
  const { data: endpoint } = useSuspenseQuery(
    endpointQuery(workspace, endpointId)
  );
  return (
    <div className="flex flex-col gap-9">
      <SettingsPanel
        description="Where events are POSTed, and which types it receives. Saving replaces the list of types."
        title="Endpoint"
      >
        <EndpointSettingsForm endpoint={endpoint} key={endpoint.id} />
      </SettingsPanel>
      <RotateSecretPanel endpoint={endpoint} />
      <SettingsPanel>
        <VerifySignatures />
      </SettingsPanel>
      <DeleteEndpointPanel endpoint={endpoint} />
    </div>
  );
};

export const Route = createFileRoute("/w/$slug/webhooks/$endpointId/settings")({
  head: () => ({ meta: [{ title: "Endpoint settings · Norbelys" }] }),
  component: EndpointSettings,
});
