import {
  Alert02Icon,
  Delete02Icon,
  Edit02Icon,
  Refresh01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { DomainObject } from "@norbelys/sdk";
import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { Copyable } from "@/components/copy";
import { DetailSection, DetailsAside } from "@/components/details";
import { PageBody, PageHeader, Section } from "@/components/page";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { DOMAIN_USES, DomainDialog } from "@/features/domains/domain-dialog";
import { DomainRecordsTable } from "@/features/domains/domain-records";
import { domainQuery, domainsKey, lastError } from "@/features/domains/queries";
import { RemoveDomainDialog } from "@/features/domains/remove-domain";
import { useAction } from "@/lib/actions";
import { formatDateTime, formatRelative, formatTimestamp } from "@/lib/format";
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

/** Intent stays editable; separate tracking keeps mail and website records intact. */
const DomainUse = ({ domain }: { domain: DomainObject }) => {
  const workspace = useWorkspace();
  const [editing, setEditing] = useState(false);
  return (
    <Section title="Domain use">
      <div className="flex items-center justify-between gap-4">
        <p>{DOMAIN_USES[domain.purpose]}</p>
        {canWrite(workspace) ? (
          <Button onClick={() => setEditing(true)} variant="secondary">
            <HugeiconsIcon icon={Edit02Icon} />
            Edit use
          </Button>
        ) : null}
      </div>
      {domain.tracking_domain ? (
        <p className="text-fg-2 text-sm">
          Custom tracking: {domain.tracking_domain.hostname}
        </p>
      ) : null}
      {editing ? (
        <DomainDialog
          domain={domain}
          onOpenChange={setEditing}
          open={editing}
        />
      ) : null}
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
 * publish with copy buttons, editable purpose and separate tracking hostname, and its dates beside them. It reads itself
 * again every few seconds while a check runs.
 */
const DomainPage = () => {
  const workspace = useWorkspace();
  const { domainId } = Route.useParams();
  const action = useAction();
  const { data: domain } = useSuspenseQuery(domainQuery(workspace, domainId));
  return (
    <PageBody>
      <PageHeader
        actions={<DomainActions domain={domain} />}
        compact
        back={{
          label: "Domains",
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
            {domain.dns_preparation === "preparing" ? (
              <p className="text-fg-2 text-sm">
                Preparing mail records and the public DKIM key. This page
                refreshes while they become ready.
              </p>
            ) : null}
            {domain.dns_preparation === "unavailable" ? (
              <Alert variant="warning">
                <AlertTitle>Mail records are not ready</AlertTitle>
                <AlertDescription>
                  Check the hosted mail configuration, then select Verify now to
                  retry. If you send through another provider, use its
                  authentication records.
                </AlertDescription>
              </Alert>
            ) : null}
            {domain.warnings.map((warning) => (
              <Alert key={warning} variant="warning">
                <AlertTitle>Existing DNS configuration</AlertTitle>
                <AlertDescription>{warning}</AlertDescription>
              </Alert>
            ))}
            <DomainRecordsTable records={domain.records} />
          </Section>
          <DomainUse domain={domain} />
          {domain.tracking_domain ? (
            <Section title={`Tracking DNS: ${domain.tracking_domain.hostname}`}>
              <p className="text-fg-2 text-sm">
                Publish these records at the tracking hostname. Its verification
                and certificate are independent of your mail domain.
              </p>
              <DomainRecordsTable records={domain.tracking_domain.records} />
              {canWrite(workspace) ? (
                <Button
                  onClick={() =>
                    action(
                      "Tracking verification started",
                      () =>
                        workspace.api.sendingDomains.verify(
                          domain.tracking_domain?.id ?? ""
                        ),
                      domainsKey(workspace)
                    )
                  }
                  variant="secondary"
                >
                  Verify tracking
                </Button>
              ) : null}
            </Section>
          ) : null}
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
                label: "Use",
                value: DOMAIN_USES[domain.purpose],
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
  head: () => ({ meta: [{ title: "Domain · Norbelys" }] }),
  component: DomainPage,
});
