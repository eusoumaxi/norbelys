import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ConnectionObject, DomainObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { ListTable } from "@/components/data-table";
import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { PageBody, PageHeader, Section } from "@/components/page";
import { Problem, SaveFailure } from "@/components/problem";
import { StatusBadge } from "@/components/status-badge";
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
import { DomainDialog } from "@/features/domains/domain-dialog";
import { DomainRecordsTable } from "@/features/domains/domain-records";
import {
  domainOptionsQuery,
  domainQuery,
  domainsKey,
  domainVerified,
  lastError,
} from "@/features/domains/queries";
import { ConnectionHealth } from "@/features/mailboxes/parts";
import {
  connectionGroupQuery,
  connectionsKey,
} from "@/features/mailboxes/queries";
import { FormField } from "@/lib/form";
import { canWrite, useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** Connect a domain with its existing workspace authorization and explicit mail purpose. */
const connectDomain = async (
  workspace: Workspace,
  domain: DomainObject
): Promise<ConnectionObject> => {
  const saved = await workspace.api.connections.create({
    provider: "norbelys",
    account_email: domain.hostname,
    identities: [],
  });
  if (!("id" in saved)) {
    throw new Error("The mail service did not return the domain connection.");
  }
  return saved;
};

const QueryProblem = ({
  query,
}: {
  query: { isError: boolean; error: unknown; refetch: () => unknown };
}) =>
  query.isError ? (
    <Problem
      error={query.error}
      onRetry={() => {
        void query.refetch();
      }}
    />
  ) : null;

/** DNS setup remains beside the address draft until ownership is verified. */
const DomainSetup = ({ domain }: { domain: DomainObject }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const lastProblem = lastError(domain);
  return (
    <section className="flex min-w-0 flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <StatusBadge kind="domain" value={domain.status} />
        <Button
          disabled={busy || domain.status === "verifying"}
          onClick={async () => {
            setBusy(true);
            setFailure(null);
            try {
              const saved = await workspace.api.sendingDomains.verify(
                domain.id
              );
              queryClient.setQueryData(
                domainQuery(workspace, domain.id).queryKey,
                saved
              );
              void queryClient.invalidateQueries({
                queryKey: domainsKey(workspace),
              });
            } catch (error) {
              setFailure(error);
            }
            setBusy(false);
          }}
          size="s"
          variant="secondary"
        >
          Verify now
        </Button>
      </div>
      <p className="text-fg-2 text-sm">
        Publish these records with your DNS provider, then verify the domain.
        Return here after publishing the records to connect your domain.
      </p>
      <DomainRecordsTable records={domain.records} />
      {domain.dns_preparation === "preparing" ? (
        <p className="text-fg-3 text-xs">
          Preparing mail authentication records…
        </p>
      ) : null}
      {domain.warnings.map((warning) => (
        <p className="text-warning text-xs" key={warning}>
          {warning}
        </p>
      ))}
      {lastProblem ? (
        <p className="text-warning text-xs">{lastProblem.detail}</p>
      ) : null}
      <SaveFailure failure={failure} />
    </section>
  );
};

/** A domain service has one delivery connection and any number of sender identities. */
export const ManagedDomainDialog = ({
  open,
  onOpenChange,
  onSaved,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSaved?: (connection: ConnectionObject) => void;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const choices = useQuery({ ...domainOptionsQuery(workspace), enabled: open });
  const [domainId, setDomainId] = useState("");
  const [domainEditor, setDomainEditor] = useState<"new" | "edit" | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const selected = useQuery({
    ...domainQuery(workspace, domainId),
    enabled: open && domainId !== "",
  });
  const domain = selected.data;
  const ready =
    canWrite(workspace) &&
    domain &&
    domainVerified(domain) &&
    domain.purpose !== "tracking";
  return (
    <>
      <Dialog
        open={open && domainEditor === null}
        onOpenChange={(next) => {
          if (!busy) {
            onOpenChange(next);
          }
        }}
      >
        <DialogContent className="max-w-[800px]">
          <form
            className="flex min-h-0 flex-col"
            onSubmit={async (event) => {
              event.preventDefault();
              if (!ready || busy || !domain) {
                return;
              }
              setBusy(true);
              setFailure(null);
              try {
                const saved = await connectDomain(workspace, domain);
                void queryClient.invalidateQueries({
                  queryKey: connectionsKey(workspace),
                });
                toast.success(
                  "Domain connected. Add senders or send with your API key."
                );
                onSaved?.(saved);
                onOpenChange(false);
              } catch (error) {
                setFailure(error);
              }
              setBusy(false);
            }}
          >
            <DialogHeader>
              <DialogTitle>Connect domain · Norbelys mail</DialogTitle>
              <DialogDescription>
                Connect your domain once. Use your workspace API key to send
                from its addresses.
              </DialogDescription>
            </DialogHeader>
            <DialogBody className="gap-4">
              <QueryProblem query={choices} />
              <FormField htmlFor="managed-domain" label="Domain">
                <Select
                  id="managed-domain"
                  disabled={choices.isPending || busy}
                  value={domainId}
                  onChange={(id) => {
                    setDomainId(id);
                    setFailure(null);
                  }}
                  options={(choices.data?.data ?? [])
                    .filter((item) => item.purpose !== "tracking")
                    .map((item) => ({ label: item.hostname, value: item.id }))}
                  placeholder={
                    choices.isPending
                      ? "Loading domains…"
                      : "Choose your domain"
                  }
                />
                <div className="flex gap-2">
                  <Button
                    disabled={busy}
                    onClick={() => setDomainEditor("new")}
                    size="s"
                    variant="secondary"
                  >
                    Add domain
                  </Button>
                  {domain ? (
                    <Button
                      disabled={busy}
                      onClick={() => setDomainEditor("edit")}
                      size="s"
                      variant="secondary"
                    >
                      Edit domain use
                    </Button>
                  ) : null}
                </div>
              </FormField>
              <QueryProblem query={selected} />
              {domain && !domainVerified(domain) ? (
                <DomainSetup domain={domain} />
              ) : null}
              <p className="text-fg-2 text-sm">
                Sender addresses share the domain&apos;s sending connection.
                Adding a sender does not create a mailbox or a password.
                Incoming mail follows your selected domain use. Existing
                mailboxes are kept.
              </p>
              <SaveFailure failure={failure} />
            </DialogBody>
            <DialogActions
              disabled={busy}
              note={
                domain && !domainVerified(domain)
                  ? "Verify your domain to connect it."
                  : undefined
              }
            >
              <SubmitButton busy={busy} disabled={!ready}>
                Connect domain
              </SubmitButton>
            </DialogActions>
          </form>
        </DialogContent>
      </Dialog>
      {domainEditor ? (
        <DomainDialog
          domain={domainEditor === "edit" ? domain : undefined}
          open={open}
          onOpenChange={(next) => {
            if (!next) {
              setDomainEditor(null);
            }
          }}
          onSaved={(saved) => {
            setDomainId(saved.id);
          }}
        />
      ) : null}
    </>
  );
};

/** Managed sending services are listed by domain; independently provisioned mailboxes stay visible. */
export const ManagedMail = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [adding, setAdding] = useState(false);
  const openConnection = (connection: ConnectionObject) => {
    void navigate({
      params: { slug: workspace.slug, connectionId: connection.id },
      to: "/w/$slug/mailboxes/$connectionId",
    });
  };
  return (
    <PageBody>
      <PageHeader
        title="Norbelys mail"
        subtitle="Connect your domain once. Send from its addresses with your workspace API key."
        actions={
          canWrite(workspace) ? (
            <Button onClick={() => setAdding(true)} variant="primary">
              <HugeiconsIcon icon={Add01Icon} />
              Connect domain
            </Button>
          ) : null
        }
        back={{
          label: "Mailboxes",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/mailboxes",
            search: {},
          },
        }}
      />
      <ListTable<ConnectionObject>
        columns={[
          {
            id: "domain",
            header: "Domain",
            render: (connection) => (
              <span className="font-semibold">{connection.account.email}</span>
            ),
          },
          {
            id: "senders",
            header: "Senders",
            render: (connection) => connection.identities.length,
          },
          {
            id: "status",
            header: "Status",
            render: (connection) => (
              <ConnectionHealth connection={connection} />
            ),
          },
        ]}
        empty={{
          title: "Connect your first domain",
          description:
            "Verify your domain, then add senders or use new addresses directly through the API.",
          icon: Add01Icon,
        }}
        onRowClick={openConnection}
        query={connectionGroupQuery(workspace, "norbelys", "")}
        rowKey={(connection) => connection.id}
      />
      <Section title="Existing mailboxes">
        <p className="text-fg-2 text-sm">
          These addresses have their own mailbox access. They are kept
          separately from domain sending services.
        </p>
        <ListTable<ConnectionObject>
          columns={[
            {
              id: "address",
              header: "Address",
              render: (connection) => connection.account.email,
            },
            {
              id: "status",
              header: "Status",
              render: (connection) => (
                <ConnectionHealth connection={connection} />
              ),
            },
          ]}
          empty={{
            title: "No separate mailboxes",
            description: "Sending addresses do not require mailbox logins.",
            icon: Add01Icon,
          }}
          onRowClick={openConnection}
          query={connectionGroupQuery(workspace, "managed-mailboxes", "")}
          rowKey={(connection) => connection.id}
        />
      </Section>
      <p className="text-fg-3 mt-4 text-xs">
        <Link
          className="text-link"
          params={{ slug: workspace.slug }}
          to="/w/$slug/domains"
        >
          Manage domains and DNS records
        </Link>
      </p>
      {adding ? (
        <ManagedDomainDialog
          open={adding}
          onOpenChange={setAdding}
          onSaved={openConnection}
        />
      ) : null}
    </PageBody>
  );
};
