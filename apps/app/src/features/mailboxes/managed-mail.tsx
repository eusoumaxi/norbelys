import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ConnectionObject, DomainObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { ListTable } from "@/components/data-table";
import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { PageBody, PageHeader } from "@/components/page";
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
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { DOMAIN_USES, DomainDialog } from "@/features/domains/domain-dialog";
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
import { fieldProblems, problemAt } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

type MailUse = "send" | "send_receive" | "receive";

/** A domain grants the directions available to its individual addresses. */
const mailUses = (domain: DomainObject | undefined) => {
  if (!domain) {
    return [];
  }
  let uses: MailUse[];
  switch (domain.purpose) {
    case "send_receive": {
      uses = ["send", "send_receive", "receive"];
      break;
    }
    case "send": {
      uses = ["send"];
      break;
    }
    case "receive": {
      uses = ["receive"];
      break;
    }
    default: {
      uses = [];
    }
  }
  return uses.map((value) => ({ label: DOMAIN_USES[value], value }));
};

/** An address is created only after ownership and its selected direction are confirmed. */
const canCreateAddress = (
  domain: DomainObject | undefined,
  localPart: string,
  use: MailUse | null
): boolean =>
  Boolean(
    domain &&
    domainVerified(domain) &&
    localPart.trim() &&
    !localPart.includes("@") &&
    use &&
    mailUses(domain).some((option) => option.value === use)
  );

const createAddress = async (
  workspace: Workspace,
  address: string,
  name: string,
  use: MailUse
): Promise<ConnectionObject> => {
  const saved = await workspace.api.connections.create({
    provider: "norbelys",
    account_email: address,
    identities: [
      {
        email: address,
        name: name.trim() || null,
        enabled: use !== "receive",
        verified: true,
      },
    ],
    receiving: { folders: use === "send" ? [] : ["INBOX"] },
  });
  if (!("id" in saved)) {
    throw new Error("The mail service did not return the new address.");
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

const addressUse = (connection: ConnectionObject): string => {
  const sends = connection.identities.some((identity) => identity.enabled);
  const receives = connection.receiving.folders.some(
    (folder) => folder.enabled
  );
  if (sends && receives) {
    return "Send and receive";
  }
  if (sends) {
    return "Send only";
  }
  return receives ? "Receive only" : "Disabled";
};

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
        Your address draft stays here while you complete setup.
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

/** Add a managed address directly; its private login and default sender are created together. */
export const ManagedSenderDialog = ({
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
  const [localPart, setLocalPart] = useState("");
  const [name, setName] = useState("");
  const [use, setUse] = useState<MailUse | null>(null);
  const [domainEditor, setDomainEditor] = useState<"new" | "edit" | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const selected = useQuery({
    ...domainQuery(workspace, domainId),
    enabled: open && domainId !== "",
  });
  const domain = selected.data;
  const options = mailUses(domain);
  const address = domain ? `${localPart.trim()}@${domain.hostname}` : "";
  const ready = canWrite(workspace) && canCreateAddress(domain, localPart, use);
  const problems = fieldProblems(failure);
  const close = (next: boolean) => {
    if (!busy) {
      onOpenChange(next);
    }
  };
  return (
    <>
      <Dialog onOpenChange={close} open={open && domainEditor === null}>
        <DialogContent className="max-w-[800px]">
          <form
            className="flex min-h-0 flex-col"
            onSubmit={async (event) => {
              event.preventDefault();
              if (!ready || busy || use === null) {
                return;
              }
              setBusy(true);
              setFailure(null);
              try {
                const saved = await createAddress(
                  workspace,
                  address,
                  name,
                  use
                );
                void queryClient.invalidateQueries({
                  queryKey: connectionsKey(workspace),
                });
                toast.success("Address added. Norbelys is setting it up.");
                onSaved?.(saved);
                onOpenChange(false);
              } catch (error) {
                setFailure(error);
              }
              setBusy(false);
            }}
          >
            <DialogHeader>
              <DialogTitle>Add sender · Norbelys mail</DialogTitle>
              <DialogDescription>
                Choose your domain and address. Norbelys configures the mail
                connection for you.
              </DialogDescription>
            </DialogHeader>
            <DialogBody className="gap-4">
              <QueryProblem query={choices} />
              <FormField htmlFor="managed-domain" label="Domain">
                <Select
                  disabled={choices.isPending || busy}
                  id="managed-domain"
                  onChange={(id) => {
                    setDomainId(id);
                    setUse(null);
                    setFailure(null);
                  }}
                  options={(choices.data?.data ?? [])
                    .filter((item) => item.purpose !== "tracking")
                    .map((item) => ({ label: item.hostname, value: item.id }))}
                  placeholder={
                    choices.isPending
                      ? "Loading domains…"
                      : "Choose a mail domain"
                  }
                  value={domainId}
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
              <FormField
                htmlFor="managed-address"
                label="Address"
                problem={
                  problemAt(problems, "account_email") ??
                  problemAt(problems, "identities.0.email")
                }
              >
                <div className="flex items-center gap-2">
                  <Input
                    autoComplete="off"
                    disabled={busy}
                    id="managed-address"
                    maxLength={64}
                    onChange={(event) => setLocalPart(event.target.value)}
                    placeholder="sales"
                    required
                    value={localPart}
                  />
                  <span className="text-fg-2 shrink-0">
                    @{domain?.hostname ?? "your-domain.com"}
                  </span>
                </div>
              </FormField>
              <FormField
                htmlFor="managed-name"
                label="Display name"
                optional
                problem={problemAt(problems, "identities.0.name")}
              >
                <Input
                  disabled={busy}
                  id="managed-name"
                  maxLength={200}
                  onChange={(event) => setName(event.target.value)}
                  placeholder="Your team"
                  value={name}
                />
              </FormField>
              <FormField
                description="Choose which directions this address uses. Sending only stops inbox collection; it does not change the domain's MX records."
                htmlFor="managed-use"
                label="Use"
                problem={problemAt(problems, "receiving")}
              >
                <Select
                  disabled={!domain || busy}
                  id="managed-use"
                  onChange={(value) => {
                    if (
                      value === "send" ||
                      value === "send_receive" ||
                      value === "receive"
                    ) {
                      setUse(value);
                    }
                  }}
                  options={options}
                  placeholder="Choose how to use this address"
                  value={use}
                />
              </FormField>
              {domain && !domainVerified(domain) ? (
                <DomainSetup domain={domain} />
              ) : null}
              <SaveFailure failure={failure} />
            </DialogBody>
            <DialogActions
              disabled={busy}
              note={
                !domain || domainVerified(domain)
                  ? undefined
                  : "Verify the domain to finish adding this address."
              }
            >
              <SubmitButton busy={busy} disabled={!ready}>
                Add sender
              </SubmitButton>
            </DialogActions>
          </form>
        </DialogContent>
      </Dialog>
      {domainEditor ? (
        <DomainDialog
          domain={domainEditor === "edit" ? domain : undefined}
          onOpenChange={(next) => {
            if (!next) {
              setDomainEditor(null);
            }
          }}
          onSaved={(saved) => {
            setDomainId(saved.id);
            setUse(null);
          }}
          open={open}
        />
      ) : null}
    </>
  );
};

/** The built-in service groups managed addresses without exposing their private connections. */
export const ManagedMail = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [adding, setAdding] = useState(false);
  const add = canWrite(workspace) ? (
    <Button onClick={() => setAdding(true)} variant="primary">
      <HugeiconsIcon icon={Add01Icon} />
      Add sender
    </Button>
  ) : null;
  return (
    <PageBody>
      <PageHeader
        actions={add}
        back={{
          label: "Mailboxes",
          link: {
            params: { slug: workspace.slug },
            to: "/w/$slug/mailboxes",
            search: {},
          },
        }}
        subtitle="Your addresses on your own domains. Manage each sender's name, signature, sending and incoming mail."
        title="Norbelys mail"
      />
      <ListTable<ConnectionObject>
        columns={[
          {
            id: "address",
            header: "Address",
            render: (connection) => (
              <span className="font-semibold">{connection.account.email}</span>
            ),
          },
          {
            id: "name",
            header: "Name",
            render: (connection) => connection.identities[0]?.name ?? "—",
          },
          {
            id: "use",
            header: "Use",
            render: (connection) => addressUse(connection),
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
          title: "Add your first sender",
          description:
            "Use an address on your domain. We prepare its connection and sender together.",
          icon: Add01Icon,
        }}
        onRowClick={(connection) => {
          void navigate({
            params: { slug: workspace.slug, connectionId: connection.id },
            to: "/w/$slug/mailboxes/$connectionId",
          });
        }}
        query={connectionGroupQuery(workspace, "norbelys", "")}
        rowKey={(connection) => connection.id}
      />
      <p className="text-fg-3 mt-4 text-xs">
        Need to change DNS or add a domain?{" "}
        <Link
          className="text-link"
          params={{ slug: workspace.slug }}
          to="/w/$slug/domains"
        >
          Manage domains
        </Link>
        .
      </p>
      {adding ? (
        <ManagedSenderDialog onOpenChange={setAdding} open={adding} />
      ) : null}
    </PageBody>
  );
};
