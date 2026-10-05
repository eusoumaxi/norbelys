import { Add01Icon, MailAccount01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ConnectionObject } from "@norbelys/sdk";
import { useInfiniteQuery } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useDeferredValue, useState } from "react";

import { ListTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { useMailboxControls } from "@/features/mailboxes/mailbox-header";
import {
  ConnectionHealth,
  MailboxCell,
  TodayMeter,
} from "@/features/mailboxes/parts";
import { warmupShare } from "@/features/mailboxes/providers";
import { connectionListQuery } from "@/features/mailboxes/queries";
import { Reveal } from "@/features/mailboxes/reveal";
import { useWorkspace } from "@/lib/workspace";

/** How many mailboxes a workspace holds before the list offers its search. */
const SEARCH_FROM = 6;

/** The primary action: the page of providers to connect an account from. */
const ConnectButton = () => {
  const workspace = useWorkspace();
  return (
    <Button
      nativeButton={false}
      render={
        <Link params={{ slug: workspace.slug }} to="/w/$slug/mailboxes/new" />
      }
      variant="primary"
    >
      <HugeiconsIcon icon={Add01Icon} />
      Connect mailbox
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

/**
 * Every account the workspace sends from (mailboxes, relays, the hosted mail), newest first: what
 * it is, whether it works, and how much of what it may send today it has sent. A row opens the
 * mailbox. Once there are enough mailboxes to look for one, a search narrows them by the start
 * of their address.
 */
const MailboxesPage = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const search = useDeferredValue(q.trim());
  // The unfiltered list the table shows first: shared, so it is read once.
  const all = useInfiniteQuery(connectionListQuery(workspace, ""));
  const first = all.data?.pages[0];
  const many =
    (first?.data.length ?? 0) >= SEARCH_FROM || Boolean(first?.meta.has_more);
  // With no mailbox at all, the empty state holds the one Connect button.
  const none = first !== undefined && first.data.length === 0;

  return (
    <PageBody>
      <PageHeader
        actions={none ? null : <ConnectButton />}
        subtitle="The email accounts Norbelys sends from. Each one sends at its own pace, up to its daily limit, and has its replies read."
        title="Mailboxes"
      />
      <div className="flex flex-col">
        <Reveal className="pb-2" open={many || q !== ""}>
          <SearchInput
            label="Search mailboxes"
            onChange={setQ}
            placeholder="Search by address…"
            value={q}
          />
        </Reveal>
        <ListTable<ConnectionObject>
          columns={[
            {
              header: "Mailbox",
              id: "mailbox",
              render: (m) => <MailboxCell connection={m} />,
            },
            {
              header: "Status",
              id: "status",
              render: (m) => <ConnectionHealth connection={m} />,
            },
            {
              header: "Today",
              id: "today",
              render: (m) => <TodayCell mailbox={m} />,
            },
            {
              className: "w-[62px]",
              header: "",
              id: "menu",
              render: (m) => <MailboxMenu mailbox={m} />,
            },
          ]}
          empty={
            search
              ? {
                  description: `No mailbox address starts with “${search}”.`,
                  icon: MailAccount01Icon,
                  title: "No mailbox found",
                }
              : {
                  action: <ConnectButton />,
                  description:
                    "Connect the mailbox you write from (Google, Microsoft or any other) and Norbelys sends from it at a person's pace.",
                  icon: MailAccount01Icon,
                  illustration: "mailbox",
                  title: "Connect your first mailbox",
                }
          }
          onRowClick={(m) => {
            void navigate({
              params: { connectionId: m.id, slug: workspace.slug },
              to: "/w/$slug/mailboxes/$connectionId",
            });
          }}
          query={connectionListQuery(workspace, search)}
          rowKey={(m) => m.id}
        />
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/mailboxes/")({
  head: () => ({ meta: [{ title: "Mailboxes · Norbelys" }] }),
  component: MailboxesPage,
});
