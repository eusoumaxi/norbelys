import { Add01Icon, FingerPrintIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";

import { Dash, DataTable } from "@/components/data-table";
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
import { Spinner } from "@/components/ui/spinner";
import {
  LinkedIdentities,
  RecoveryCodes,
} from "@/features/account/security-panels";
import { useAction } from "@/lib/actions";
import { useRefreshMe, useSession } from "@/lib/auth";
import { FormField } from "@/lib/form";
import { formatRelative, humanize } from "@/lib/format";
import { describeProblem } from "@/lib/problem";
import type { Grant, Passkey, SessionInfo } from "@/lib/session";
import { createPasskey, passkeysSupported } from "@/lib/webauthn";

const TABS = ["profile", "security", "apps"] as const;
type Tab = (typeof TABS)[number];
const LABELS: Record<Tab, string> = {
  apps: "Connected apps",
  profile: "Profile",
  security: "Security",
};

// First match wins: Edge and Chrome also name Safari in their user agents.
const BROWSERS: [RegExp, string][] = [
  [/Edg\//u, "Edge"],
  [/Chrome\//u, "Chrome"],
  [/Firefox\//u, "Firefox"],
  [/Safari\//u, "Safari"],
];
const SYSTEMS: [RegExp, string][] = [
  [/iPhone|iPad/u, "iOS"],
  [/Mac OS X/u, "macOS"],
  [/Windows/u, "Windows"],
  [/Android/u, "Android"],
  [/Linux/u, "Linux"],
];

/** A short name for a browser from its user agent: enough to tell sessions apart. */
const browserName = (agent?: string | null) => {
  if (!agent) {
    return "Unknown device";
  }
  const browser =
    BROWSERS.find(([pattern]) => pattern.test(agent))?.[1] ?? "Browser";
  const os = SYSTEMS.find(([pattern]) => pattern.test(agent))?.[1];
  return os ? `${browser} on ${os}` : browser;
};

const Account = () => {
  const session = useSession();
  const { tab = "profile" } = Route.useSearch();
  const [name, setName] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [registering, setRegistering] = useState(false);
  const { me } = session;
  const refresh = useRefreshMe();
  const action = useAction();
  const saveProfile = async () => {
    setSaving(true);
    await action(
      "Profile saved",
      () => session.updateMe({ name: name?.trim() || null }),
      async () => {
        setName(null);
        await refresh();
      }
    );
    setSaving(false);
  };

  const addPasskey = async () => {
    setRegistering(true);
    try {
      const challenge = await session.startPasskeyRegistration();
      const credential = await createPasskey(challenge.options);
      if (credential) {
        await session.addPasskey(
          challenge.id,
          credential,
          browserName(navigator.userAgent)
        );
        toast.success("Passkey added");
        await refresh();
      }
    } catch (error) {
      toast.error(
        error instanceof DOMException
          ? "This browser could not create a passkey for Norbelys here."
          : describeProblem(error).detail
      );
    }
    setRegistering(false);
  };

  const act = (label: string, run: () => Promise<unknown>) =>
    action(label, run, refresh);

  return (
    <SettingsLayout
      header={
        <header className="flex flex-col gap-0.5">
          <h1 className="text-fg text-3xl font-semibold">Account settings</h1>
          <p className="text-fg-3 text-sm">{me.email}</p>
        </header>
      }
      tabs={TABS.map((id) => ({
        active: id === tab,
        label: LABELS[id],
        link: { search: id === "profile" ? {} : { tab: id }, to: "/account" },
      }))}
    >
      {tab === "profile" ? (
        <>
          <ReadOnlyFields
            fields={[
              { label: "User ID", value: me.id },
              { label: "Email", value: me.email },
            ]}
          />
          <SettingsPanel
            description="How teammates see you in their workspaces."
            title="Profile"
          >
            <form
              className="flex flex-col gap-4"
              onSubmit={(event) => {
                event.preventDefault();
                void saveProfile();
              }}
            >
              <FormField
                className="max-w-[388px]"
                htmlFor="me-name"
                label="Name"
              >
                <Input
                  id="me-name"
                  onChange={(event) => setName(event.target.value)}
                  placeholder="Your name"
                  value={name ?? me.name ?? ""}
                />
              </FormField>
              <div>
                <Button
                  disabled={name === null || saving}
                  type="submit"
                  variant="secondary"
                >
                  {saving ? <Spinner /> : null}
                  Save
                </Button>
              </div>
            </form>
          </SettingsPanel>
        </>
      ) : null}
      {tab === "security" ? (
        <>
          <SettingsPanel
            description="Sign in with your fingerprint, face or a security key instead of an email code."
            title="Passkeys"
          >
            {passkeysSupported() ? (
              <div>
                <Button
                  disabled={registering}
                  onClick={() => {
                    void addPasskey();
                  }}
                  variant="secondary"
                >
                  {registering ? (
                    <Spinner />
                  ) : (
                    <HugeiconsIcon icon={Add01Icon} />
                  )}
                  Add passkey
                </Button>
              </div>
            ) : null}
            <DataTable<Passkey>
              columns={[
                {
                  render: (p) => (
                    <span className="flex items-center gap-2 font-bold">
                      <HugeiconsIcon
                        className="text-icon size-4"
                        icon={FingerPrintIcon}
                      />
                      {p.name}
                    </span>
                  ),
                  header: "Name",
                  id: "name",
                },
                {
                  render: (p) => formatRelative(p.created_at),
                  header: "Added",
                  id: "added",
                },
                {
                  render: (p) =>
                    p.last_used_at ? formatRelative(p.last_used_at) : <Dash />,
                  header: "Last used",
                  id: "used",
                },
                {
                  render: (p) => (
                    <RowMenu>
                      <DropdownMenuItem
                        className="text-error-fg"
                        onClick={() =>
                          act("Passkey removed", () =>
                            session.deletePasskey(p.id)
                          )
                        }
                      >
                        Remove
                      </DropdownMenuItem>
                    </RowMenu>
                  ),
                  className: "w-[62px]",
                  header: "",
                  id: "menu",
                },
              ]}
              empty={{
                description: "You sign in with a code sent to your email.",
                icon: FingerPrintIcon,
                title: "No passkeys",
              }}
              rowKey={(p) => p.id}
              rows={me.passkeys}
            />
          </SettingsPanel>
          <LinkedIdentities />
          <RecoveryCodes />
          <SettingsPanel
            description="Browsers signed in to your account. Ending a session signs that browser out at once."
            title="Sessions"
          >
            <DataTable<SessionInfo>
              columns={[
                {
                  render: (s) => (
                    <span className="flex items-center gap-2 font-bold">
                      {browserName(s.user_agent)}
                      {s.current ? (
                        <Badge tone="success">This browser</Badge>
                      ) : null}
                    </span>
                  ),
                  header: "Device",
                  id: "device",
                },
                {
                  render: (s) => humanize(s.auth_method),
                  header: "Signed in with",
                  id: "method",
                },
                {
                  render: (s) => formatRelative(s.last_seen_at),
                  header: "Last active",
                  id: "seen",
                },
                {
                  render: (s) => formatRelative(s.expires_at),
                  header: "Expires",
                  id: "expires",
                },
                {
                  render: (s) =>
                    s.current ? null : (
                      <RowMenu>
                        <DropdownMenuItem
                          className="text-error-fg"
                          onClick={() =>
                            act("Session ended", () =>
                              session.revokeSession(s.id)
                            )
                          }
                        >
                          End session
                        </DropdownMenuItem>
                      </RowMenu>
                    ),
                  className: "w-[62px]",
                  header: "",
                  id: "menu",
                },
              ]}
              rowKey={(s) => s.id}
              rows={me.sessions}
            />
          </SettingsPanel>
        </>
      ) : null}
      {tab === "apps" ? (
        <SettingsPanel
          description="AI agents (MCP) and the CLI you approved, each acting in one workspace. Revoking stops them within a minute."
          title="Connected apps"
        >
          <DataTable<Grant>
            columns={[
              {
                render: (g) => (
                  <span className="font-bold">{g.client_name}</span>
                ),
                header: "App",
                id: "app",
              },
              {
                render: (g) =>
                  session.memberships.find(
                    (m) => m.workspace.id === g.workspace_id
                  )?.workspace.name ?? (
                    <code className="font-mono text-xs">{g.workspace_id}</code>
                  ),
                header: "Workspace",
                id: "workspace",
              },
              {
                render: (g) => (
                  <span className="flex flex-wrap gap-1">
                    {g.scopes.map((scope) => (
                      <Badge key={scope}>{scope}</Badge>
                    ))}
                  </span>
                ),
                header: "Access",
                id: "scopes",
              },
              {
                render: (g) =>
                  g.last_used_at ? formatRelative(g.last_used_at) : <Dash />,
                header: "Last used",
                id: "used",
              },
              {
                render: (g) => (
                  <RowMenu>
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() =>
                        act("Access revoked", () => session.revokeGrant(g.id))
                      }
                    >
                      Revoke access
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
                "Connect an AI agent with the MCP server, or sign in with `norbelys login`.",
              icon: FingerPrintIcon,
              title: "No connected apps",
            }}
            rowKey={(g) => g.id}
            rows={me.grants}
          />
        </SettingsPanel>
      ) : null}
    </SettingsLayout>
  );
};

export const Route = createFileRoute("/_account/account")({
  validateSearch: z.object({ tab: z.enum(TABS).optional() }),
  head: () => ({ meta: [{ title: "Account settings · Norbelys" }] }),
  component: Account,
});
