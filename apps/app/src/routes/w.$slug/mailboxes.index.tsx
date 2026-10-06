import { Add01Icon, MailAccount01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ConnectionObject } from "@norbelys/sdk";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { parseAsString, useQueryState } from "nuqs";
import { useDeferredValue, useState } from "react";

import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader, Section } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { useMailboxControls } from "@/features/mailboxes/mailbox-header";
import { ManagedMail } from "@/features/mailboxes/managed-mail";
import {
  ConnectionHealth,
  MailboxCell,
  TodayMeter,
} from "@/features/mailboxes/parts";
import { warmupShare } from "@/features/mailboxes/providers";
import { connectionGroupQuery } from "@/features/mailboxes/queries";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The primary action: the page of providers to connect an account from. */
const ConnectButton = () => {
  const workspace = useWorkspace();
  if (!canWrite(workspace)) {
    return null;
  }
  return (
    <Button
      nativeButton={false}
      render={
        <Link params={{ slug: workspace.slug }} to="/w/$slug/mailboxes/new" />
      }
      variant="primary"
    >
      <HugeiconsIcon icon={Add01Icon} />
      Connect account
    </Button>
  );
};

/** A row's quick actions: check now, pause or resume, copy the id. */
const MailboxMenu = ({ mailbox }: { mailbox: ConnectionObject }) => {
  const controls = useMailboxControls(mailbox);
  return (
    <RowMenu>
      {mailbox.status === "archived" ? null : (
        <>
          <DropdownMenuItem onClick={controls.handleCheck}>
            Check now
          </DropdownMenuItem>
          <DropdownMenuItem onClick={controls.handleTogglePause}>
            {controls.pauseLabel}
          </DropdownMenuItem>
        </>
      )}
      <CopyIdItem id={mailbox.id} noun="mailbox" />
    </RowMenu>
  );
};

/** Today's count against what the mailbox may send today, and its warm-up while it warms. */
const TodayCell = ({ mailbox }: { mailbox: ConnectionObject }) => (
  <span className="flex flex-col gap-0.5">
    <TodayMeter connection={mailbox} />
    {mailbox.warmup_stage === null ||
    mailbox.warmup_stage === undefined ? null : (
      <span className="text-fg-3 text-xs">
        Warming up: {warmupShare(mailbox.warmup_stage)}% of its limit
      </span>
    )}
  </span>
);

/** Personal mailboxes and sending services have distinct setup and sender management. */
const MailboxesPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const search = useDeferredValue(q.trim());
  const [service, setService] = useQueryState(
    "service",
    parseAsString.withOptions({ history: "push" })
  );
  if (service === "norbelys") {
    return <ManagedMail />;
  }
  const columns = [
    {
      header: "Account",
      id: "account",
      render: (m: ConnectionObject) => <MailboxCell connection={m} />,
    },
    {
      header: "Status",
      id: "status",
      render: (m: ConnectionObject) => <ConnectionHealth connection={m} />,
    },
    {
      header: "Today",
      id: "today",
      render: (m: ConnectionObject) => <TodayCell mailbox={m} />,
    },
    {
      className: "w-[62px]",
      header: "",
      id: "menu",
      render: (m: ConnectionObject) => <MailboxMenu mailbox={m} />,
    },
  ];
  const openAccount = (connection: ConnectionObject) => {
    void navigate({
      params: { connectionId: connection.id, slug: workspace.slug },
      to: "/w/$slug/mailboxes/$connectionId",
    });
  };
  return (
    <PageBody>
      <PageHeader
        actions={<ConnectButton />}
        subtitle="Connect your own mailbox or use a sending service. Manage each account's authorized senders in one place."
        title="Mailboxes"
      />
      <div className="flex flex-col gap-6">
        <SearchInput
          label="Search accounts"
          onChange={setQ}
          placeholder="Search by address or account…"
          value={q}
        />
        <Section title="Your mailboxes">
          <p className="text-fg-2 text-sm">
            Gmail, Microsoft and personal SMTP mailboxes. Connecting one creates
            its main sender automatically.
          </p>
          <ListTable<ConnectionObject>
            columns={columns}
            empty={{
              description: search
                ? `No mailbox address starts with “${search}”.`
                : "Connect your mailbox to send as yourself and collect replies.",
              icon: MailAccount01Icon,
              title: search ? "No mailbox found" : "No personal mailboxes yet",
            }}
            onRowClick={openAccount}
            query={connectionGroupQuery(workspace, "mailboxes", search)}
            rowKey={(m) => m.id}
          />
        </Section>
        <Section title="Sending services">
          <button
            className="border-line hover:bg-hover focus-visible:outline-focus flex w-full cursor-pointer items-center justify-between gap-4 rounded-sm border p-4 text-left outline-none focus-visible:outline-1"
            onClick={() => {
              void setService("norbelys");
            }}
            type="button"
          >
            <span className="flex flex-col gap-1">
              <span className="font-semibold">Norbelys mail</span>
              <span className="text-fg-2 text-sm">
                Connect your domain once and send from its addresses with your
                workspace API key.
              </span>
            </span>
            <span className="text-link shrink-0 text-sm">Manage domains</span>
          </button>
          <ListTable<ConnectionObject>
            columns={columns}
            empty={{
              description: search
                ? `No service account starts with “${search}”.`
                : "SES, SendGrid and Mailgun: connect an account once, then add its authorized senders.",
              icon: MailAccount01Icon,
              title: search
                ? "No service found"
                : "No external sending services yet",
            }}
            onRowClick={openAccount}
            query={connectionGroupQuery(workspace, "services", search)}
            rowKey={(m) => m.id}
          />
        </Section>
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/mailboxes/")({
  validateSearch: (search): { service?: "norbelys" } => {
    if (search.service === "norbelys") {
      return { service: "norbelys" };
    }
    return {};
  },
  head: () => ({ meta: [{ title: "Mailboxes · Norbelys" }] }),
  component: MailboxesPage,
});
