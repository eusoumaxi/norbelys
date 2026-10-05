import { Add01Icon, Globe02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { DomainObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { CreateDialog } from "@/components/create-dialog";
import { Dash, ListTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { domainListQuery, domainsKey } from "@/features/domains/queries";
import { RemoveDomainDialog } from "@/features/domains/remove-domain";
import { useAction } from "@/lib/actions";
import { formatCount, formatRelative } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** `2 of 3 found`: the records the last check found. */
const recordsText = (domain: DomainObject) => {
  const found = domain.records.filter(
    (record) => record.status === "verified"
  ).length;
  return `${formatCount(found)} of ${formatCount(domain.records.length)} found`;
};

/**
 * The workspace's sending domains, newest first: each one's status, whether it serves tracking
 * links and how many of its DNS records the last check found. A row opens the domain; Add domain
 * creates one and opens it on its records.
 */
const DomainsPage = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const action = useAction();
  const [creating, setCreating] = useState(false);
  const [removing, setRemoving] = useState<DomainObject | null>(null);
  const key = domainsKey(workspace);
  const open = (domain: DomainObject) => {
    void navigate({
      params: { domainId: domain.id, slug: workspace.slug },
      to: "/w/$slug/domains/$domainId",
    });
  };
  const addButton = (
    <Button onClick={() => setCreating(true)} variant="primary">
      <HugeiconsIcon icon={Add01Icon} />
      Add domain
    </Button>
  );

  return (
    <PageBody>
      <PageHeader actions={addButton} title="Sending domains" />
      <ListTable<DomainObject>
        columns={[
          {
            header: "Domain",
            id: "hostname",
            render: (d) => <NameCell icon={Globe02Icon}>{d.hostname}</NameCell>,
          },
          {
            header: "Status",
            id: "status",
            render: (d) => <StatusBadge kind="domain" value={d.status} />,
          },
          {
            header: "Tracking",
            id: "tracking",
            render: (d) =>
              d.tracking_enabled ? (
                <Badge tone="info">Tracking</Badge>
              ) : (
                <Dash />
              ),
          },
          { header: "DNS records", id: "records", render: recordsText },
          {
            header: "Last checked",
            id: "checked",
            render: (d) =>
              d.checked_at ? formatRelative(d.checked_at) : <Dash />,
          },
          {
            className: "w-[62px]",
            header: "",
            id: "menu",
            render: (d) => (
              <RowMenu>
                <DropdownMenuItem onClick={() => open(d)}>
                  DNS records
                </DropdownMenuItem>
                <DropdownMenuItem
                  onClick={() =>
                    action(
                      "Verification started",
                      () => workspace.api.sendingDomains.verify(d.id),
                      key
                    )
                  }
                >
                  Verify now
                </DropdownMenuItem>
                <CopyIdItem id={d.id} noun="domain" />
                <DropdownMenuItem
                  className="text-error-fg"
                  onClick={() => setRemoving(d)}
                >
                  Remove
                </DropdownMenuItem>
              </RowMenu>
            ),
          },
        ]}
        empty={{
          action: addButton,
          description:
            "Verify a domain with a DNS record to send from it with the hosted mail, and to serve tracking links from your own hostname.",
          icon: Globe02Icon,
          illustration: "domain",
          title: "No sending domains",
        }}
        onRowClick={open}
        query={domainListQuery(workspace)}
        rowKey={(d) => d.id}
      />
      <CreateDialog
        description="You get the DNS records to publish; the check runs on its own once they resolve."
        fields={[
          {
            label: "Domain",
            mono: true,
            name: "hostname",
            placeholder: "mail.example.com",
            required: true,
          },
          {
            description:
              "Its CNAME then points at Norbelys, and links are served from it once its certificate is issued.",
            label: "Tracking links",
            name: "tracking_enabled",
            options: [
              { label: "Only prove ownership", value: "no" },
              { label: "Also serve tracking links from it", value: "yes" },
            ],
          },
        ]}
        onOpenChange={setCreating}
        onSubmit={async (values) => {
          const domain = await workspace.api.sendingDomains.create({
            hostname: values.hostname ?? "",
            tracking_enabled: values.tracking_enabled === "yes",
          });
          await queryClient.invalidateQueries({ queryKey: key });
          setCreating(false);
          open(domain);
        }}
        open={creating}
        submitLabel="Add domain"
        title="Add sending domain"
      />
      <RemoveDomainDialog
        domain={removing}
        onOpenChange={(next) => {
          if (!next) {
            setRemoving(null);
          }
        }}
        open={removing !== null}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/domains/")({
  head: () => ({ meta: [{ title: "Sending domains · Norbelys" }] }),
  component: DomainsPage,
});
