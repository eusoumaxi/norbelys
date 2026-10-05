import { Add01Icon, Key01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CodeLine } from "@/components/copy";
import { CreateDialog } from "@/components/create-dialog";
import {
  Dash,
  dashboardListQuery,
  ListTable,
  NameCell,
} from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { RowMenu } from "@/components/row-menu";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { ConnectPanel } from "@/features/developers/connect-panel";
import { useAction } from "@/lib/actions";
import { formatRelative } from "@/lib/format";
import type { ApiKey } from "@/lib/session";
import { canAdminister, useWorkspace } from "@/lib/workspace";

/**
 * What a key may do: its first scope, then how many more, all of them listed on hover. A key acts
 * with these scopes intersected with its creator's current ones.
 */
const Scopes = ({ scopes }: { scopes: string[] }) => {
  const [first, ...rest] = scopes;
  if (!first) {
    return <Dash />;
  }
  return (
    <span className="flex items-center gap-1.5">
      <Badge className="font-mono">{first}</Badge>
      {rest.length > 0 ? (
        <Tooltip>
          <TooltipTrigger render={<span />}>
            <Badge tone="muted">+{rest.length} more</Badge>
          </TooltipTrigger>
          <TooltipContent>
            <ul className="font-mono">
              {scopes.map((scope) => (
                <li key={scope}>{scope}</li>
              ))}
            </ul>
          </TooltipContent>
        </Tooltip>
      ) : null}
    </span>
  );
};

/**
 * The workspace's API keys: for servers, scripts and CI. A secret is shown once, at creation;
 * owners and admins rename and revoke keys from a row's menu, and each key shows its scopes.
 */
const ApiKeys = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const action = useAction();
  const { new: creating } = Route.useSearch();
  const [secret, setSecret] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<ApiKey | null>(null);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  const admin = canAdminister(workspace);
  const key = [workspace.id, "api_keys"] as const;
  const setCreating = (open: boolean) => {
    void navigate({
      params: { slug: workspace.slug },
      replace: true,
      search: open ? { new: true } : {},
      to: "/w/$slug/api-keys",
    });
  };

  return (
    <PageBody>
      <PageHeader
        actions={
          admin ? (
            <Button onClick={() => setCreating(true)} variant="primary">
              <HugeiconsIcon icon={Add01Icon} />
              Create API key
            </Button>
          ) : null
        }
        title="API keys"
      />
      {secret ? (
        <Alert className="mb-5" variant="success">
          <AlertTitle>Your new API key</AlertTitle>
          <AlertDescription className="flex flex-col gap-2">
            Copy it now and keep it in your environment as NORBELYS_API_KEY: it
            is shown once.
            <CodeLine prefix={null} value={secret} />
          </AlertDescription>
        </Alert>
      ) : null}
      <ListTable<ApiKey>
        columns={[
          {
            render: (k) => <NameCell icon={Key01Icon}>{k.name}</NameCell>,
            header: "Name",
            id: "name",
          },
          {
            render: (k) => (
              <code className="text-fg-2 font-mono text-xs">{k.prefix}…</code>
            ),
            header: "Key",
            id: "prefix",
          },
          {
            render: (k) => <Scopes scopes={k.scopes} />,
            header: "Scopes",
            id: "scopes",
          },
          {
            render: (k) => <StatusBadge kind="key" value={k.status} />,
            header: "Status",
            id: "status",
          },
          {
            render: (k) =>
              k.last_used_at ? formatRelative(k.last_used_at) : <Dash />,
            header: "Last used",
            id: "used",
          },
          {
            render: (k) =>
              k.expires_at ? formatRelative(k.expires_at) : "Never",
            header: "Expires",
            id: "expires",
          },
          {
            render: (k) =>
              admin ? (
                <RowMenu>
                  <DropdownMenuItem onClick={() => setRenaming(k)}>
                    Rename
                  </DropdownMenuItem>
                  {k.status === "active" ? (
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() => setRevoking(k)}
                    >
                      Revoke
                    </DropdownMenuItem>
                  ) : null}
                </RowMenu>
              ) : null,
            className: "w-[62px]",
            header: "",
            id: "menu",
          },
        ]}
        empty={{
          action: admin ? (
            <Button onClick={() => setCreating(true)} variant="primary">
              <HugeiconsIcon icon={Add01Icon} />
              Create API key
            </Button>
          ) : undefined,
          description:
            "An API key lets your code, the SDK or the CLI act in this workspace. Keep it on servers, never in a browser.",
          icon: Key01Icon,
          illustration: "key",
          title: "No API keys yet",
        }}
        query={dashboardListQuery<ApiKey>(
          workspace,
          [...key, "list"],
          "/api_keys"
        )}
        rowKey={(k) => k.id}
      />
      <CreateDialog
        description="It acts with this workspace's authority while you remain a member. Set an expiry for keys used by tools you might forget."
        fields={[
          {
            label: "Name",
            name: "name",
            placeholder: "Production server",
            required: true,
          },
          {
            description: "Leave empty for a key that does not expire.",
            label: "Expires",
            name: "expires_at",
            type: "datetime-local",
          },
        ]}
        onOpenChange={setCreating}
        onSubmit={async (values) => {
          const created = await workspace.session.workspace<ApiKey>(
            workspace.id,
            "POST",
            "/api_keys",
            {
              body: {
                expires_at: values.expires_at
                  ? new Date(values.expires_at).toISOString()
                  : undefined,
                name: values.name,
              },
              idempotent: true,
            }
          );
          setSecret(created.secret ?? null);
          await queryClient.invalidateQueries({ queryKey: key });
          setCreating(false);
        }}
        open={Boolean(creating)}
        submitLabel="Create key"
        title="Create API key"
      />
      {renaming ? (
        <CreateDialog
          description={`Only its name changes: ${renaming.prefix}… keeps working.`}
          fields={[
            {
              initial: renaming.name,
              label: "Name",
              name: "name",
              required: true,
            },
          ]}
          key={renaming.id}
          onOpenChange={(open) => {
            if (!open) {
              setRenaming(null);
            }
          }}
          onSubmit={async (values) => {
            await workspace.session.workspace<ApiKey>(
              workspace.id,
              "PATCH",
              `/api_keys/${renaming.id}`,
              {
                body: { name: values.name },
                headers: { "If-Match": `"${renaming.version}"` },
              }
            );
            toast.success("Key renamed");
            await queryClient.invalidateQueries({ queryKey: key });
            setRenaming(null);
          }}
          open
          submitLabel="Rename"
          title="Rename API key"
        />
      ) : null}
      <ConfirmDialog
        confirmLabel="Revoke key"
        danger
        description={`Requests signed with ${revoking?.prefix ?? "this key"}… are refused from now on. This can't be undone.`}
        onConfirm={() =>
          revoking
            ? action(
                "Key revoked",
                () =>
                  workspace.session.workspace(
                    workspace.id,
                    "DELETE",
                    `/api_keys/${revoking.id}`
                  ),
                key
              )
            : undefined
        }
        onOpenChange={(open) => {
          if (!open) {
            setRevoking(null);
          }
        }}
        open={revoking !== null}
        title={`Revoke ${revoking?.name ?? "this key"}?`}
      />
      <div className="mt-8">
        <ConnectPanel createKey={false} workspace={workspace} />
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/api-keys")({
  validateSearch: z.object({ new: z.boolean().optional() }),
  head: () => ({ meta: [{ title: "API keys · Norbelys" }] }),
  component: ApiKeys,
});
