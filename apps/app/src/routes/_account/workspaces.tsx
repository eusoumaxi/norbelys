import { Add01Icon, Building03Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { Dash, DataTable } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { SearchInput } from "@/components/search-input";
import { WorkspaceMark } from "@/components/shell/workspace-switcher";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { useSession } from "@/lib/auth";
import { formatDate, humanize } from "@/lib/format";
import type { Membership } from "@/lib/session";

/** The way to create a workspace: its own page. */
const NewWorkspaceButton = () => (
  <Button render={<Link to="/new" />} variant="primary">
    <HugeiconsIcon icon={Add01Icon} />
    New workspace
  </Button>
);

/**
 * Every workspace the person belongs to, reached from the workspace menu ("Manage workspaces"):
 * their role in each, a filter, and the way to create one. The dashboard itself opens on a
 * workspace's overview, never here.
 */
const Workspaces = () => {
  const session = useSession();
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const { memberships } = session;
  const needle = q.trim().toLowerCase();
  const rows = needle
    ? memberships.filter(
        (m) =>
          m.workspace.name.toLowerCase().includes(needle) ||
          m.workspace.slug.toLowerCase().includes(needle)
      )
    : memberships;

  return (
    <PageBody>
      <PageHeader
        actions={<NewWorkspaceButton />}
        subtitle="Each workspace keeps its own mailboxes, people, campaigns and keys. Agencies use one per client."
        title="Workspaces"
      />
      <div className="flex flex-col gap-4">
        <SearchInput label="Search workspaces" onChange={setQ} value={q} />
        <DataTable<Membership>
          columns={[
            {
              render: (m) => (
                <span className="flex items-center gap-2.5">
                  <WorkspaceMark className="size-6" name={m.workspace.name} />
                  <span className="text-fg font-medium">
                    {m.workspace.name}
                  </span>
                </span>
              ),
              header: "Name",
              id: "name",
            },
            {
              render: (m) => humanize(m.role),
              header: "Your role",
              id: "role",
            },
            {
              render: (m) =>
                m.status === "active" ? formatDate(m.created_at) : <Dash />,
              header: "Member since",
              id: "since",
            },
            {
              render: (m) => (
                <RowMenu>
                  <DropdownMenuItem
                    onClick={() => {
                      void navigate({
                        params: { slug: m.workspace.slug },
                        to: "/w/$slug",
                      });
                    }}
                  >
                    Open
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onClick={() => {
                      void navigate({
                        params: { slug: m.workspace.slug },
                        to: "/w/$slug/settings",
                      });
                    }}
                  >
                    Settings
                  </DropdownMenuItem>
                  <CopyIdItem id={m.workspace.id} noun="workspace" />
                </RowMenu>
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            action: needle ? undefined : <NewWorkspaceButton />,
            description: needle
              ? "No workspace matches this search."
              : "Create a workspace to connect mailboxes, import people and send your first campaign.",
            icon: Building03Icon,
            title: needle ? "No results" : "No workspaces yet",
          }}
          onRowClick={(m) => {
            void navigate({
              params: { slug: m.workspace.slug },
              to: "/w/$slug",
            });
          }}
          rowKey={(m) => m.id}
          rows={rows}
        />
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/_account/workspaces")({
  head: () => ({ meta: [{ title: "Workspaces · Norbelys" }] }),
  component: Workspaces,
});
