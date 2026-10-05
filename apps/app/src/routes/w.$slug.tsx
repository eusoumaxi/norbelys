import { createFileRoute, notFound, Outlet } from "@tanstack/react-router";

import { NotFound } from "@/components/not-found";
import { AppShell } from "@/components/shell/app-shell";
import { requireSession } from "@/lib/auth";
import { rememberWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

const WorkspaceLayout = () => {
  const { workspace } = Route.useRouteContext();
  return (
    <AppShell workspace={workspace}>
      <Outlet />
    </AppShell>
  );
};

/**
 * Every page of one workspace, addressed by its slug (`/w/acme/campaigns`). The person's active
 * membership names it; requests carry workspace tokens minted from the session for its id.
 */
export const Route = createFileRoute("/w/$slug")({
  beforeLoad: ({ context, location, params }) => {
    const session = requireSession({ context, location });
    const membership = session.membership(params.slug);
    if (!membership) {
      throw notFound();
    }
    const workspace: Workspace = {
      api: session.client(membership.workspace.id),
      id: membership.workspace.id,
      mode: membership.workspace.mode,
      name: membership.workspace.name,
      role: membership.role,
      session,
      slug: membership.workspace.slug,
    };
    rememberWorkspace(workspace.slug);
    return { workspace };
  },
  head: () => ({ meta: [{ title: "Norbelys" }] }),
  component: WorkspaceLayout,
  notFoundComponent: NotFound,
});
