import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { EndpointObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Outlet } from "@tanstack/react-router";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import { PageBody, PageHeader } from "@/components/page";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { TabLinks } from "@/components/ui/tabs";
import { EndpointStatusBadge } from "@/features/webhooks/badges";
import { EndpointActions } from "@/features/webhooks/endpoint-actions";
import { endpointQuery } from "@/features/webhooks/queries";
import {
  formatDateTime,
  formatRelative,
  formatTimestamp,
  humanize,
  plural,
} from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** Why an endpoint was disabled, in the words of what happened. */
const DISABLED_REASONS: Record<string, string> = {
  failing:
    "No delivery succeeded for five days, so it was disabled and its pending deliveries stopped.",
  gone: "It answered 410 Gone, so it was disabled at once and its pending deliveries stopped.",
  manual: "It was disabled by hand; its pending deliveries stopped.",
};

/**
 * The banner over a troubled endpoint: disabled (and why, and how to catch up), or failing
 * since its first failure after a success (and what happens if it keeps failing).
 */
const EndpointNotice = ({ endpoint }: { endpoint: EndpointObject }) => {
  if (!endpoint.enabled) {
    const reason = endpoint.disabled_reason ?? "manual";
    return (
      <Alert
        className="mb-5"
        variant={reason === "manual" ? "neutral" : "error"}
      >
        <HugeiconsIcon icon={Alert02Icon} />
        <AlertTitle>Disabled</AlertTitle>
        <AlertDescription>
          {DISABLED_REASONS[reason] ?? `Disabled: ${humanize(reason)}.`} Events
          that happen while it is disabled are not sent to it: enable it, then
          replay from when it stopped to send what it missed.
        </AlertDescription>
      </Alert>
    );
  }
  if (endpoint.failing_since) {
    return (
      <Alert className="mb-5" variant="warning">
        <HugeiconsIcon icon={Alert02Icon} />
        <AlertTitle>
          Failing since {formatDateTime(endpoint.failing_since)}
        </AlertTitle>
        <AlertDescription>
          No delivery has succeeded since then. Deliveries keep retrying on
          their schedule; after five days without a success the endpoint is
          disabled, and the workspace&apos;s admins are told by email.
        </AlertDescription>
      </Alert>
    );
  }
  return null;
};

/**
 * One webhook endpoint: its URL and state in the header with its actions (a test event, a
 * replay, enable or disable), a banner when it is failing or disabled, its sections as tabs
 * (deliveries, settings) and its details beside them.
 */
const EndpointPage = () => {
  const workspace = useWorkspace();
  const { endpointId } = Route.useParams();
  const { data: endpoint } = useSuspenseQuery(
    endpointQuery(workspace, endpointId)
  );
  const params = { endpointId, slug: workspace.slug };
  return (
    <PageBody>
      <PageHeader
        actions={<EndpointActions endpoint={endpoint} />}
        compact
        back={{
          label: "Webhooks",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/webhooks",
          },
        }}
        subtitle={<EndpointStatusBadge endpoint={endpoint} />}
        title={
          <span className="font-mono text-2xl font-medium break-all">
            {endpoint.url}
          </span>
        }
      />
      <EndpointNotice endpoint={endpoint} />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-5">
          <TabLinks
            tabs={[
              {
                exact: true,
                label: "Deliveries",
                link: { params, to: "/w/$slug/webhooks/$endpointId" },
              },
              {
                label: "Settings",
                link: { params, to: "/w/$slug/webhooks/$endpointId/settings" },
              },
            ]}
          />
          <Outlet />
        </div>
        <DetailsAside>
          <DetailSection
            rows={[
              { label: "ID", value: <Copyable mono value={endpoint.id} /> },
              {
                label: "Status",
                value: <EndpointStatusBadge endpoint={endpoint} />,
              },
              {
                label: "Events",
                value: plural(endpoint.event_types.length, "type"),
              },
              ...(endpoint.failing_since
                ? [
                    {
                      label: "Failing since",
                      value: formatRelative(endpoint.failing_since),
                    },
                  ]
                : []),
              ...(endpoint.disabled_reason
                ? [
                    {
                      label: "Disabled",
                      value: humanize(endpoint.disabled_reason),
                    },
                  ]
                : []),
            ]}
            title="Endpoint"
          />
          <DetailSection
            rows={[
              { label: "Created", value: formatTimestamp(endpoint.created_at) },
              { label: "Updated", value: formatTimestamp(endpoint.updated_at) },
              { label: "Version", value: String(endpoint.version) },
            ]}
            title="History"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/webhooks/$endpointId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      endpointQuery(context.workspace, params.endpointId)
    ),
  head: () => ({ meta: [{ title: "Webhook endpoint · Norbelys" }] }),
  component: EndpointPage,
});
