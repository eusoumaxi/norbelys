import { CheckmarkCircle02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { AuthLayout } from "@/components/auth-layout";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Select } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useSession } from "@/lib/auth";
import { describeProblem } from "@/lib/problem";
import { consentDetails } from "@/lib/session";

/** What each scope lets the app do, in the words of the API's own scope list. */
const SCOPES: Record<string, string> = {
  "analytics:read": "Read reports",
  "automation:manage": "Manage webhooks, events and jobs",
  "automation:read": "Read webhooks, events and jobs",
  "campaigns:read": "Read campaigns and enrollments",
  "campaigns:write": "Create and change campaigns, enrollments and images",
  "connections:manage": "Connect, change and remove mailboxes and domains",
  "connections:read": "Read mailboxes, sending limits and domains",
  "inbox:read": "Read conversations and received mail",
  "inbox:write": "Update and review conversations",
  "messages:read": "Read messages and delivery events",
  "messages:send": "Send, cancel and resolve messages",
  "people:read": "Read people, groups, segments and suppressions",
  "people:write": "Change people, groups, segments, imports and suppressions",
  "workspace:manage": "Manage the workspace's settings, members and keys",
  "workspace:read": "Read the workspace",
};

/** Validate external callbacks before a browser navigation can interpret their scheme. */
const safeCallback = (uri: string): string => {
  const target = new URL(uri);
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(
    target.hostname
  );
  if (
    (target.protocol !== "https:" &&
      !(target.protocol === "http:" && loopback)) ||
    target.username ||
    target.password ||
    target.hash
  ) {
    throw new Error("The application returned an unsafe callback URL.");
  }
  return target.href;
};

/**
 * One OAuth decision, for the MCP consent page (`request`) and the CLI's device approval
 * (`userCode`): who asks, for what, in which workspace; approving creates a grant the person can
 * revoke from their account settings.
 */
export const Consent = ({
  request,
  userCode,
}: {
  request?: string;
  userCode?: string;
}) => {
  const session = useSession();
  const details = useQuery({
    queryFn: () => consentDetails({ request, user_code: userCode }),
    queryKey: ["oauth", "consent", request ?? userCode],
    retry: false,
  });
  const { memberships } = session;
  const [workspaceId, setWorkspaceId] = useState<string | null>(
    memberships[0]?.workspace.id ?? null
  );
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [done, setDone] = useState<"approved" | "denied" | null>(null);

  const decide = async (approve: boolean) => {
    setBusy(true);
    setProblem(null);
    try {
      const answer = await session.decideConsent({
        approve,
        request,
        user_code: userCode,
        workspace_id: approve ? (workspaceId ?? undefined) : undefined,
      });
      if (answer.redirect_to) {
        window.location.assign(safeCallback(answer.redirect_to));
        return;
      }
      setDone(answer.approved ? "approved" : "denied");
    } catch (error) {
      setProblem(describeProblem(error).detail);
    }
    setBusy(false);
  };

  if (done) {
    return (
      <AuthLayout
        subtitle={
          done === "approved"
            ? "You can close this page and return to your terminal."
            : "Nothing was granted. You can close this page."
        }
        title={done === "approved" ? "Device approved" : "Access denied"}
      >
        <HugeiconsIcon
          className="text-accent mx-auto size-10"
          icon={CheckmarkCircle02Icon}
        />
      </AuthLayout>
    );
  }

  if (details.isError) {
    return (
      <AuthLayout
        subtitle={describeProblem(details.error).detail}
        title="Request not valid"
      >
        <p className="text-fg-3 text-center text-sm">
          It may have expired: start again from the app that sent you here.
        </p>
      </AuthLayout>
    );
  }

  const info = details.data;
  return (
    <AuthLayout
      subtitle={
        info ? (
          <>
            <span className="text-fg">{info.client_name}</span> wants to act in
            one of your workspaces
            {info.redirect_host ? (
              <>
                {" "}
                and will return you to{" "}
                <span className="text-fg font-mono">{info.redirect_host}</span>
              </>
            ) : null}
            .
          </>
        ) : (
          "Loading the request…"
        )
      }
      title={userCode ? "Approve this device" : "Allow access"}
    >
      {info ? (
        <div className="flex flex-col gap-6">
          {info.user_code ? (
            <p className="border-line bg-chrome text-fg rounded-sm border py-3 text-center font-mono text-xl tracking-[0.3em]">
              {info.user_code}
            </p>
          ) : null}
          <div className="flex flex-col gap-1.5">
            <Label>It will be able to</Label>
            <ul className="border-line flex flex-col rounded-sm border">
              {info.scopes.map((scope) => (
                <li
                  className="border-line text-fg-2 border-b px-3 py-2 text-sm last:border-b-0"
                  key={scope}
                >
                  {SCOPES[scope] ?? scope}
                </li>
              ))}
            </ul>
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="consent-workspace">Workspace</Label>
            <Select
              id="consent-workspace"
              onChange={setWorkspaceId}
              options={memberships.map((m) => ({
                label: m.workspace.name,
                value: m.workspace.id,
              }))}
              value={workspaceId}
            />
          </div>
          {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
          <div className="flex flex-col gap-2">
            <Button
              disabled={busy || !workspaceId}
              onClick={() => {
                void decide(true);
              }}
              variant="primary"
            >
              {busy ? <Spinner /> : null}
              {userCode ? "Approve device" : "Allow"}
            </Button>
            <Button
              disabled={busy}
              onClick={() => {
                void decide(false);
              }}
              variant="tertiary"
            >
              Deny
            </Button>
          </div>
        </div>
      ) : (
        <Skeleton className="h-48 w-full" />
      )}
    </AuthLayout>
  );
};
