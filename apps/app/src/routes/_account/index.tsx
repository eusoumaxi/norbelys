import { createFileRoute, redirect } from "@tanstack/react-router";

import { homeWorkspace } from "@/lib/workspace";

/**
 * The dashboard's front door. It has no page of its own: a person lands on the overview of the
 * workspace this browser opened last (or their first one), and a new account, which has none yet,
 * on creating it. Every workspace is listed at `/workspaces`.
 */
export const Route = createFileRoute("/_account/")({
  beforeLoad: ({ context }) => {
    const slug = homeWorkspace(
      context.session.memberships.map((m) => m.workspace.slug)
    );
    if (slug) {
      throw redirect({ params: { slug }, replace: true, to: "/w/$slug" });
    }
    throw redirect({ replace: true, to: "/new" });
  },
});
