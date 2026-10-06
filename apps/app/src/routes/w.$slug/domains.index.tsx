import { Add01Icon, Globe02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { DomainObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { Dash, ListTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { DOMAIN_USES, DomainDialog } from "@/features/domains/domain-dialog";
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
      <PageHeader actions={addButton} title="Domains" />
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
            header: "Use",
            id: "purpose",
            render: (d) => DOMAIN_USES[d.purpose],
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
            "Choose sending, receiving, tracking, or a combination, then publish the records for your choices.",
          icon: Globe02Icon,
          illustration: "domain",
          title: "No domains",
        }}
        onRowClick={open}
        query={domainListQuery(workspace)}
        rowKey={(d) => d.id}
      />
      {creating ? (
        <DomainDialog
          onOpenChange={setCreating}
          onSaved={open}
          open={creating}
        />
      ) : null}
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
  head: () => ({ meta: [{ title: "Domains · Norbelys" }] }),
  component: DomainsPage,
});
