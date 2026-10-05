import type { Norbelys, WorkspaceMode } from "@norbelys/sdk";
import { queryOptions } from "@tanstack/react-query";
import { useRouteContext } from "@tanstack/react-router";

import { humanize } from "@/lib/format";
import type { MembershipRole, Session } from "@/lib/session";

/** The workspace in use and the client that acts in it with the person's session. */
export interface Workspace {
  /** The public SDK, sending this session's workspace tokens. */
  api: Norbelys;
  /** `ws_…`: first in every query key, so switching workspaces never mixes their data. */
  id: string;
  slug: string;
  name: string;
  mode: WorkspaceMode;
  role: MembershipRole;
  session: Session;
}

export const useWorkspace = (): Workspace =>
  useRouteContext({ from: "/w/$slug", select: (context) => context.workspace });

/** Whether the person may change the workspace's settings, members and keys. */
export const canAdminister = (workspace: Workspace): boolean =>
  workspace.role === "owner" || workspace.role === "admin";

/**
 * Whether the person may change the workspace's data (campaigns, people, messages, mailboxes):
 * everyone but a viewer, who only reads.
 */
export const canWrite = (workspace: Workspace): boolean =>
  workspace.role !== "viewer";

/** The roles a member can be given, as choices; an owner is whoever created the workspace. */
export const ROLE_OPTIONS = (["admin", "member", "viewer"] as const).map(
  (role) => ({ label: humanize(role), value: role })
);

/** The workspace as `workspaces.retrieve` reads it: its time zone, its dates and its usage. */
export const workspaceQuery = (workspace: Workspace) =>
  queryOptions({
    queryFn: ({ signal }) =>
      workspace.api.workspaces.retrieve(workspace.id, { signal }),
    queryKey: [workspace.id, "workspace"],
  });

const LAST_WORKSPACE = "nb.workspace.last";

/** Notes the workspace this browser opened last, so the dashboard's front door returns to it. */
export const rememberWorkspace = (slug: string) => {
  try {
    localStorage.setItem(LAST_WORKSPACE, slug);
  } catch {
    // Storage refused (a private window, a full quota): the first workspace is opened instead.
  }
};

/**
 * Where the dashboard's front door leads: the workspace this browser opened last while the person
 * is still a member of it, else their first one; `null` when they have none yet.
 */
export const homeWorkspace = (slugs: readonly string[]): string | null => {
  let last: string | null = null;
  try {
    last = localStorage.getItem(LAST_WORKSPACE);
  } catch {
    last = null;
  }
  if (last && slugs.includes(last)) {
    return last;
  }
  return slugs[0] ?? null;
};
