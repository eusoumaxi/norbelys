import {
  Alert02Icon,
  Delete02Icon,
  Refresh01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { DomainObject } from "@norbelys/sdk";
import { useQueryClient, useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import { PageBody, PageHeader, Section } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { DomainRecordsTable } from "@/features/domains/domain-records";
import { domainQuery, domainsKey, lastError } from "@/features/domains/queries";
import { RemoveDomainDialog } from "@/features/domains/remove-domain";
import { useAction } from "@/lib/actions";
import {
  formatDateTime,
  formatRelative,
  formatTimestamp,
  onOff,
} from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** What each status means for the person, in a sentence. */
const STATUS_TEXT: Record<string, string> = {
  active: "Proven, and its tracking hostname serves links.",
  pending_certificate:
    "Proven, and its tracking CNAME points at Norbelys: its certificate is being issued.",
  pending_verification:
    "Not proven yet: publish the ownership record, then verify.",
  suspended:
    "Proven once, but its ownership record is gone: the hostname stays yours until the record is back or the domain is removed.",
  verified: "Proven: its ownership record resolves.",
  verifying:
    "Its DNS records are being checked; this page refreshes on its own.",
};

/** The last check's complaint, as the API recorded it. */
const LastError = ({ domain }: { domain: DomainObject }) => {
  const error = lastError(domain);
  if (!error) {
    return null;
  }
  return (
    <Alert variant="warning">
      <HugeiconsIcon icon={Alert02Icon} />
      <AlertTitle>
        {error.at
          ? `The check of ${formatDateTime(error.at)} found a problem`
          : "The last check found a problem"}
      </AlertTitle>
      <AlertDescription>{error.detail}</AlertDescription>
    </Alert>
  );
};

/** The switch that serves tracking links from the domain (`sending_domains.update`). */
const Tracking = ({ domain }: { domain: DomainObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  const [busy, setBusy] = useState(false);
  const change = async (on: boolean) => {
    setBusy(true);
    await action(
      on ? "Tracking turned on" : "Tracking turned off",
      async () => {
        const saved = await workspace.api.sendingDomains.update(domain.id, {
          tracking_enabled: on,
        });
        queryClient.setQueryData(
          domainQuery(workspace, domain.id).queryKey,
          saved
        );
      },
      () => {
        void queryClient.invalidateQueries({ queryKey: domainsKey(workspace) });
      }
    );
    setBusy(false);
  };
  return (
    <Section title="Tracking links">
      <Card>
        <CardContent className="flex flex-col gap-2">
          <Label className="flex items-center gap-2 text-sm font-semibold">
            <Switch
              checked={domain.tracking_enabled}
              disabled={busy || !canWrite(workspace)}
              onCheckedChange={(on) => {
                void change(on);
              }}
            />
            Serve open and click tracking from {domain.hostname}
          </Label>
          <p className="text-fg-2 text-sm">
            The hostname then needs a CNAME to Norbelys (it joins the records
            above). Once a check finds it, a certificate is issued and the
            domain becomes Active; only then do campaigns that name it serve
            their links from it.
          </p>
        </CardContent>
      </Card>
    </Section>
  );
};

/** Verify now and Remove, in the header. */
const DomainActions = ({ domain }: { domain: DomainObject }) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const action = useAction();
  const [removing, setRemoving] = useState(false);
  if (!canWrite(workspace)) {
    return null;
  }
  return (
    <>
      <Button
        onClick={() =>
          action(
            "Verification started",
            () => workspace.api.sendingDomains.verify(domain.id),
            domainsKey(workspace)
          )
        }
        variant="secondary"
      >
        <HugeiconsIcon icon={Refresh01Icon} />
        Verify now
      </Button>
      <Button onClick={() => setRemoving(true)} variant="danger-secondary">
        <HugeiconsIcon icon={Delete02Icon} />
        Remove
      </Button>
      <RemoveDomainDialog
        domain={domain}
        onOpenChange={setRemoving}
        onRemoved={() =>
          navigate({
            params: { slug: workspace.slug },
            to: "/w/$slug/domains",
          })
        }
        open={removing}
      />
    </>
  );
};

/**
 * One sending domain: its status and what it means, the last check's problem, the DNS records to
 * publish with copy buttons, the tracking switch, and its dates beside them. It reads itself
 * again every few seconds while a check runs.
 */
const DomainPage = () => {
  const workspace = useWorkspace();
  const { domainId } = Route.useParams();
  const { data: domain } = useSuspenseQuery(domainQuery(workspace, domainId));
  return (
    <PageBody>
      <PageHeader
        actions={<DomainActions domain={domain} />}
        compact
        back={{
          label: "Sending domains",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/domains",
          },
        }}
        subtitle={<StatusBadge kind="domain" value={domain.status} />}
        title={<span className="font-mono break-all">{domain.hostname}</span>}
      />
      <div className="flex flex-col gap-8 lg:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-8">
          <LastError domain={domain} />
          <Section title="DNS records">
            <p className="text-fg-2 text-sm">
              {STATUS_TEXT[domain.status] ?? null} Publish these records at your
              DNS provider; proven domains are checked again every day, and
              Verify now checks at once.
            </p>
            <DomainRecordsTable records={domain.records} />
          </Section>
          <Tracking domain={domain} />
        </div>
        <DetailsAside>
          <DetailSection
            rows={[
              {
                label: "Status",
                value: <StatusBadge kind="domain" value={domain.status} />,
              },
              {
                label: "Verified",
                value: domain.verified_at
                  ? formatTimestamp(domain.verified_at)
                  : "Not yet",
              },
              {
                label: "Last checked",
                value: domain.checked_at
                  ? formatRelative(domain.checked_at)
                  : "Not yet",
              },
              {
                label: "Tracking",
                value: onOff(domain.tracking_enabled),
              },
            ]}
            title="Domain"
          />
          <DetailSection
            rows={[
              { label: "Added", value: formatTimestamp(domain.created_at) },
              { label: "Updated", value: formatTimestamp(domain.updated_at) },
              { label: "ID", value: <Copyable mono value={domain.id} /> },
            ]}
            title="History"
          />
        </DetailsAside>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/domains/$domainId")({
  loader: ({ context, params }) =>
    context.queryClient.ensureQueryData(
      domainQuery(context.workspace, params.domainId)
    ),
  head: () => ({ meta: [{ title: "Sending domain · Norbelys" }] }),
  component: DomainPage,
});
