import { Add01Icon, Shield01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { Copyable } from "@/components/copy";
import { CreateDialog } from "@/components/create-dialog";
import {
  dashboardListQuery,
  listedItem,
  ListTable,
} from "@/components/data-table";
import { RowMenu } from "@/components/row-menu";
import { SettingsPanel } from "@/components/settings-layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { useAction } from "@/lib/actions";
import { formatRelative } from "@/lib/format";
import { ROLE_OPTIONS } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** A workspace's single sign-on connection, as the API describes it. */
interface SsoConnection {
  id: string;
  name: string;
  issuer: string;
  client_id: string;
  client_secret_set: boolean;
  default_role: string;
  domains: {
    domain: string;
    record: { name: string; type: string; value: string };
    verified_at?: string | null;
  }[];
  enforced: boolean;
  jit_provisioning: boolean;
  status: string;
  status_detail?: string | null;
  policy_version: number;
  metadata_fetched_at?: string | null;
  created_at: string;
  updated_at: string;
}

/** The TXT records that prove each email domain, with their state. */
const DomainProofs = ({ connection }: { connection: SsoConnection }) => (
  <Table shell>
    <TableHeader>
      <TableRow>
        <TableHead>Domain</TableHead>
        <TableHead>Record</TableHead>
        <TableHead>Value</TableHead>
        <TableHead>State</TableHead>
      </TableRow>
    </TableHeader>
    <TableBody>
      {connection.domains.map((proof) => (
        <TableRow key={proof.domain}>
          <TableCell className="font-semibold">{proof.domain}</TableCell>
          <TableCell className="max-w-[220px]">
            <Copyable
              mono
              value={`${proof.record.type} ${proof.record.name}`}
            />
          </TableCell>
          <TableCell className="max-w-[260px]">
            <Copyable mono value={proof.record.value} />
          </TableCell>
          <TableCell>
            {proof.verified_at ? (
              <Badge dot tone="success">
                Verified
              </Badge>
            ) : (
              <Badge dot tone="warning">
                Waiting for DNS
              </Badge>
            )}
          </TableCell>
        </TableRow>
      ))}
    </TableBody>
  </Table>
);

/** The DNS records of the connection whose records were asked for. */
const OpenProofs = ({
  id,
  workspace,
}: {
  id: string;
  workspace: Workspace;
}) => {
  const queryClient = useQueryClient();
  const connection = listedItem<SsoConnection>(
    queryClient,
    [workspace.id, "sso_connections", "list"],
    id
  )?.item;
  if (!connection) {
    return null;
  }
  return (
    <SettingsPanel
      description={`Publish these TXT records for ${connection.name}; the check runs on its own, or with Check now.`}
      title="Domain records"
    >
      <DomainProofs connection={connection} />
      {connection.status_detail ? (
        <p className="text-fg-3 text-xs">{connection.status_detail}</p>
      ) : null}
    </SettingsPanel>
  );
};

/**
 * Single sign-on through any OpenID Connect provider, routed by email domain: each domain is
 * proven with a TXT record, people can be created on first sign-in (just-in-time), and enforcing
 * it makes every member sign in through the provider. Owners and admins manage it.
 */
export const SsoSettings = ({ workspace }: { workspace: Workspace }) => {
  const queryClient = useQueryClient();
  const act = useAction();
  const [creating, setCreating] = useState(false);
  const [removing, setRemoving] = useState<SsoConnection | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const key = [workspace.id, "sso_connections"] as const;
  const call = (method: string, path: string, body?: unknown) =>
    workspace.session.workspace(
      workspace.id,
      method,
      `/sso_connections${path}`,
      {
        body,
      }
    );

  return (
    <>
      <SettingsPanel
        description="Let people sign in with your identity provider (Okta, Entra ID, Google Workspace or any OpenID Connect issuer). Prove each email domain with a DNS record; enforcing it then requires that sign-in for every member, and changing it signs them in again."
        title="Single sign-on"
      >
        <div>
          <Button onClick={() => setCreating(true)} variant="secondary">
            <HugeiconsIcon icon={Add01Icon} />
            Add connection
          </Button>
        </div>
        <ListTable<SsoConnection>
          columns={[
            {
              header: "Name",
              id: "name",
              render: (c) => (
                <span className="flex min-w-0 flex-col">
                  <span className="text-fg font-bold">{c.name}</span>
                  <span className="text-fg-3 truncate font-mono text-xs">
                    {c.issuer}
                  </span>
                </span>
              ),
            },
            {
              header: "Domains",
              id: "domains",
              render: (c) => c.domains.map((d) => d.domain).join(", "),
            },
            {
              header: "State",
              id: "state",
              render: (c) => (
                <span className="flex flex-wrap gap-1.5">
                  <Badge
                    dot
                    tone={c.status === "active" ? "success" : "warning"}
                  >
                    {c.status === "active" ? "Active" : "Pending"}
                  </Badge>
                  {c.enforced ? <Badge tone="info">Enforced</Badge> : null}
                </span>
              ),
            },
            {
              header: "New people",
              id: "jit",
              render: (c) =>
                c.jit_provisioning
                  ? `Join as ${c.default_role}`
                  : "Invitation only",
            },
            {
              header: "Updated",
              id: "updated",
              render: (c) => formatRelative(c.updated_at),
            },
            {
              className: "w-[62px]",
              header: "",
              id: "menu",
              render: (c) => (
                <RowMenu>
                  <DropdownMenuItem
                    onClick={() => setOpen(open === c.id ? null : c.id)}
                  >
                    {open === c.id ? "Hide DNS records" : "DNS records"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onClick={() =>
                      act("Checked", () => call("POST", `/${c.id}/verify`), key)
                    }
                  >
                    Check now
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onClick={() =>
                      act(
                        c.enforced ? "No longer enforced" : "Enforced",
                        () =>
                          call("PATCH", `/${c.id}`, { enforced: !c.enforced }),
                        key
                      )
                    }
                  >
                    {c.enforced ? "Stop enforcing" : "Enforce for every member"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onClick={() =>
                      act(
                        c.jit_provisioning
                          ? "Invitation only"
                          : "New people join on sign-in",
                        () =>
                          call("PATCH", `/${c.id}`, {
                            jit_provisioning: !c.jit_provisioning,
                          }),
                        key
                      )
                    }
                  >
                    {c.jit_provisioning
                      ? "Require an invitation"
                      : "Let new people join"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    className="text-error-fg"
                    onClick={() => setRemoving(c)}
                  >
                    Delete
                  </DropdownMenuItem>
                </RowMenu>
              ),
            },
          ]}
          empty={{
            description:
              "Add your identity provider's issuer, client id and secret.",
            icon: Shield01Icon,
            title: "No single sign-on yet",
          }}
          query={dashboardListQuery<SsoConnection>(
            workspace,
            [...key, "list"],
            "/sso_connections"
          )}
          rowKey={(c) => c.id}
        />
      </SettingsPanel>
      {open ? <OpenProofs id={open} workspace={workspace} /> : null}
      <CreateDialog
        description="The issuer is the provider's OpenID Connect address; the redirect address to register at the provider is the API's /v1/auth/callback."
        fields={[
          { label: "Name", name: "name", placeholder: "Okta", required: true },
          {
            label: "Issuer",
            mono: true,
            name: "issuer",
            placeholder: "https://acme.okta.com",
            required: true,
            type: "url",
          },
          { label: "Client ID", mono: true, name: "client_id", required: true },
          { label: "Client secret", mono: true, name: "client_secret" },
          {
            description: "Comma-separated; each is proven with a DNS record.",
            label: "Email domains",
            name: "domains",
            placeholder: "acme.com, acme.io",
            required: true,
          },
          {
            initial: "member",
            label: "Role of people who join on sign-in",
            name: "default_role",
            options: ROLE_OPTIONS,
          },
        ]}
        onOpenChange={setCreating}
        onSubmit={async (values) => {
          await call("POST", "", {
            client_id: values.client_id,
            client_secret: values.client_secret,
            default_role: values.default_role,
            domains: (values.domains ?? "")
              .split(",")
              .map((domain) => domain.trim())
              .filter(Boolean),
            issuer: values.issuer,
            jit_provisioning: true,
            name: values.name,
          });
          await queryClient.invalidateQueries({ queryKey: key });
          setCreating(false);
        }}
        open={creating}
        submitLabel="Add connection"
        title="Add single sign-on"
      />
      <ConfirmDialog
        confirmLabel="Delete connection"
        danger
        description="People who sign in through it will need another method; an enforcing workspace stops requiring it."
        onConfirm={() => {
          if (removing) {
            act(
              "Connection deleted",
              () => call("DELETE", `/${removing.id}`),
              key
            );
          }
        }}
        onOpenChange={(value) => {
          if (!value) {
            setRemoving(null);
          }
        }}
        open={removing !== null}
        title={`Delete ${removing?.name ?? "connection"}?`}
      />
    </>
  );
};
