import {
  Add01Icon,
  ShieldUserIcon,
  UserIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CreateDialog } from "@/components/create-dialog";
import {
  ContactCell,
  Dash,
  dashboardListQuery,
  ListTable,
} from "@/components/data-table";
import { RowMenu } from "@/components/row-menu";
import {
  ReadOnlyFields,
  SettingsLayout,
  SettingsPanel,
} from "@/components/settings-layout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { SsoSettings } from "@/features/settings/sso";
import { useAction } from "@/lib/actions";
import { useRefreshMe } from "@/lib/auth";
import { FormField, TIME_ZONES } from "@/lib/form";
import { formatRelative, formatTimestamp, humanize } from "@/lib/format";
import type { AuditEntry, Invitation, Member } from "@/lib/session";
import {
  canAdminister,
  ROLE_OPTIONS,
  useWorkspace,
  workspaceQuery,
} from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

const TABS = ["general", "members", "sso", "audit-log"] as const;
type Tab = (typeof TABS)[number];
const TAB_LABELS: Record<Tab, string> = {
  "audit-log": "Audit log",
  sso: "Single sign-on",
  general: "General",
  members: "Members",
};

const General = ({ workspace }: { workspace: Workspace }) => {
  const queryClient = useQueryClient();
  const refreshMe = useRefreshMe();
  const navigate = useNavigate();
  const action = useAction();
  const details = useQuery(workspaceQuery(workspace));
  const [name, setName] = useState<string | null>(null);
  const [timezone, setTimezone] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const admin = canAdminister(workspace);
  const currentName = name ?? workspace.name;
  const currentZone = timezone ?? details.data?.timezone ?? "";
  const changed =
    (name !== null && name.trim() !== workspace.name) ||
    (timezone !== null && timezone.trim() !== details.data?.timezone);

  const save = async () => {
    setSaving(true);
    await action(
      "Workspace saved",
      () =>
        workspace.session.workspace(workspace.id, "PATCH", "", {
          body: {
            name: name?.trim() || undefined,
            timezone: timezone?.trim() || undefined,
          },
        }),
      async () => {
        setName(null);
        setTimezone(null);
        await Promise.all([
          refreshMe(),
          queryClient.invalidateQueries({
            queryKey: workspaceQuery(workspace).queryKey,
          }),
        ]);
      }
    );
    setSaving(false);
  };

  return (
    <>
      <SettingsPanel
        description="Its name, as everyone in it sees it, and the time zone new campaigns start with."
        title="Workspace"
      >
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <div className="flex flex-col gap-4 sm:flex-row sm:gap-6">
            <FormField className="flex-1" htmlFor="ws-name" label="Name">
              <Input
                disabled={!admin}
                id="ws-name"
                onChange={(event) => setName(event.target.value)}
                value={currentName}
              />
            </FormField>
            <FormField className="flex-1" htmlFor="ws-zone" label="Time zone">
              <Select
                disabled={!admin}
                id="ws-zone"
                onChange={setTimezone}
                options={(TIME_ZONES.includes(currentZone) || !currentZone
                  ? TIME_ZONES
                  : [currentZone, ...TIME_ZONES]
                ).map((zone) => ({
                  label: zone.replaceAll("_", " "),
                  value: zone,
                }))}
                value={currentZone}
              />
            </FormField>
          </div>
          {admin ? (
            <div>
              <Button
                disabled={!changed || saving}
                type="submit"
                variant="secondary"
              >
                {saving ? <Spinner /> : null}
                Save
              </Button>
            </div>
          ) : null}
        </form>
      </SettingsPanel>
      <SettingsPanel
        description={
          workspace.mode === "test"
            ? "A test workspace, created through the API: its messages are recorded but never leave for real recipients. Use these to connect your code."
            : "What your code needs to talk to this workspace through the API."
        }
        title="For developers"
      >
        <ReadOnlyFields
          fields={[
            { label: "Workspace ID", value: workspace.id },
            { label: "Address (slug)", value: workspace.slug },
          ]}
        />
      </SettingsPanel>
      {workspace.role === "owner" ? (
        <SettingsPanel
          description="Deleting a workspace stops its campaigns and erases its data after the retention period. Members lose access at once."
          title="Delete workspace"
        >
          <div>
            <Button
              onClick={() => setDeleting(true)}
              variant="danger-secondary"
            >
              Delete workspace
            </Button>
          </div>
          <ConfirmDialog
            confirmLabel="Delete workspace"
            confirmText={workspace.slug}
            danger
            description="This cannot be undone."
            onConfirm={() =>
              action(
                "Workspace scheduled for deletion",
                () => workspace.session.workspace(workspace.id, "DELETE", ""),
                async () => {
                  await refreshMe();
                  await navigate({ to: "/" });
                }
              )
            }
            onOpenChange={setDeleting}
            open={deleting}
            title={`Delete ${workspace.name}?`}
          />
        </SettingsPanel>
      ) : null}
    </>
  );
};

const Members = ({ workspace }: { workspace: Workspace }) => {
  const queryClient = useQueryClient();
  const [inviting, setInviting] = useState(false);
  const admin = canAdminister(workspace);
  const membersKey = [workspace.id, "members"] as const;
  const invitationsKey = [workspace.id, "invitations"] as const;
  const action = useAction();

  return (
    <>
      <SettingsPanel
        description="Owners and admins manage members, keys and settings; members work with the workspace's data; viewers only read."
        title="Members"
      >
        {admin ? (
          <div>
            <Button onClick={() => setInviting(true)} variant="secondary">
              <HugeiconsIcon icon={Add01Icon} />
              Invite member
            </Button>
          </div>
        ) : null}
        <ListTable<Member>
          columns={[
            {
              render: (m) => (
                <ContactCell email={m.user.email} name={m.user.name} />
              ),
              header: "Member",
              id: "user",
            },
            {
              render: (m) => (
                <Badge tone={m.role === "owner" ? "info" : "neutral"}>
                  {humanize(m.role)}
                </Badge>
              ),
              header: "Role",
              id: "role",
            },
            {
              render: (m) =>
                m.status === "active" ? (
                  humanize(m.source)
                ) : (
                  <Badge tone="warning">{humanize(m.status)}</Badge>
                ),
              header: "Joined by",
              id: "source",
            },
            {
              render: (m) => formatRelative(m.created_at),
              header: "Since",
              id: "since",
            },
            {
              render: (m) =>
                admin && m.role !== "owner" ? (
                  <RowMenu>
                    {ROLE_OPTIONS.filter((role) => role.value !== m.role).map(
                      (role) => (
                        <DropdownMenuItem
                          key={role.value}
                          onClick={() =>
                            action(
                              `Role changed to ${role.label}`,
                              () =>
                                workspace.session.workspace(
                                  workspace.id,
                                  "PATCH",
                                  `/members/${m.id}`,
                                  {
                                    body: { role: role.value },
                                  }
                                ),
                              membersKey
                            )
                          }
                        >
                          Make {role.value}
                        </DropdownMenuItem>
                      )
                    )}
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() =>
                        action(
                          "Member removed",
                          () =>
                            workspace.session.workspace(
                              workspace.id,
                              "DELETE",
                              `/members/${m.id}`
                            ),
                          membersKey
                        )
                      }
                    >
                      Remove from workspace
                    </DropdownMenuItem>
                  </RowMenu>
                ) : null,
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          query={dashboardListQuery<Member>(
            workspace,
            [...membersKey, "list"],
            "/members"
          )}
          rowKey={(m) => m.id}
        />
      </SettingsPanel>
      {admin ? (
        <SettingsPanel
          description="Invitations last 10 days; sending one again extends it."
          title="Pending invitations"
        >
          <ListTable<Invitation>
            columns={[
              {
                render: (i) => (
                  <span className="text-fg font-bold">{i.email}</span>
                ),
                header: "Email",
                id: "email",
              },
              { render: (i) => humanize(i.role), header: "Role", id: "role" },
              {
                render: (i) => formatRelative(i.expires_at),
                header: "Expires",
                id: "expires",
              },
              {
                render: (i) => (
                  <RowMenu>
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() =>
                        action(
                          "Invitation revoked",
                          () =>
                            workspace.session.workspace(
                              workspace.id,
                              "DELETE",
                              `/invitations/${i.id}`
                            ),
                          invitationsKey
                        )
                      }
                    >
                      Revoke
                    </DropdownMenuItem>
                  </RowMenu>
                ),
                className: "w-[62px]",
                header: "",
                id: "menu",
              },
            ]}
            empty={{
              description:
                "Invite teammates by email; they join when they sign in with that address.",
              icon: UserIcon,
              title: "No pending invitations",
            }}
            query={dashboardListQuery<Invitation>(
              workspace,
              [...invitationsKey, "list"],
              "/invitations",
              { status: "pending" }
            )}
            rowKey={(i) => i.id}
          />
        </SettingsPanel>
      ) : null}
      <CreateDialog
        fields={[
          {
            label: "Email",
            name: "email",
            placeholder: "teammate@example.com",
            required: true,
            type: "email",
          },
          { label: "Role", name: "role", options: ROLE_OPTIONS },
        ]}
        onOpenChange={setInviting}
        onSubmit={async (values) => {
          await workspace.session.workspace(
            workspace.id,
            "POST",
            "/invitations",
            {
              body: {
                invitations: [{ email: values.email, role: values.role }],
              },
              idempotent: true,
            }
          );
          toast.success("Invitation sent");
          await queryClient.invalidateQueries({ queryKey: invitationsKey });
          setInviting(false);
        }}
        open={inviting}
        submitLabel="Send invitation"
        title="Invite member"
      />
    </>
  );
};

const AuditLog = ({ workspace }: { workspace: Workspace }) => (
  <SettingsPanel
    description="Every change to members, keys, single sign-on and access, with who made it. Kept for 180 days."
    title="Audit log"
  >
    <ListTable<AuditEntry>
      columns={[
        {
          render: (e) => (
            <code className="text-fg font-mono text-xs font-medium">
              {e.action}
            </code>
          ),
          header: "Action",
          id: "action",
        },
        {
          render: (e) => (
            <span className="text-fg-2">
              {humanize(e.actor.kind)}{" "}
              <code className="text-fg-3 font-mono text-xs">{e.actor.id}</code>
            </span>
          ),
          header: "Actor",
          id: "actor",
        },
        {
          render: (e) =>
            e.target ? (
              <code className="text-fg-3 font-mono text-xs">{e.target}</code>
            ) : (
              <Dash />
            ),
          header: "Target",
          id: "target",
        },
        {
          render: (e) => formatTimestamp(e.created_at),
          header: "When",
          id: "when",
        },
      ]}
      empty={{
        description: "Access changes in this workspace will be listed here.",
        icon: ShieldUserIcon,
        title: "Nothing recorded yet",
      }}
      query={dashboardListQuery<AuditEntry>(
        workspace,
        [workspace.id, "audit_log", "list"],
        "/audit_log"
      )}
      rowKey={(e) => e.id}
    />
  </SettingsPanel>
);

const Settings = () => {
  const workspace = useWorkspace();
  const { tab = "general" } = Route.useSearch();
  const visible = canAdminister(workspace)
    ? TABS
    : TABS.filter((t) => t !== "audit-log" && t !== "sso");
  return (
    <SettingsLayout
      header={
        <header className="flex flex-col gap-0.5">
          <h1 className="text-fg text-3xl font-semibold">Settings</h1>
        </header>
      }
      tabs={visible.map((id) => ({
        active: id === tab,
        label: TAB_LABELS[id],
        link: {
          params: { slug: workspace.slug },
          search: id === "general" ? {} : { tab: id },
          to: "/w/$slug/settings",
        },
      }))}
    >
      {tab === "members" ? <Members workspace={workspace} /> : null}
      {tab === "audit-log" ? <AuditLog workspace={workspace} /> : null}
      {tab === "sso" ? <SsoSettings workspace={workspace} /> : null}
      {tab === "general" ? <General workspace={workspace} /> : null}
    </SettingsLayout>
  );
};

export const Route = createFileRoute("/w/$slug/settings")({
  validateSearch: z.object({ tab: z.enum(TABS).optional() }),
  head: () => ({ meta: [{ title: "Settings · Norbelys" }] }),
  component: Settings,
});
