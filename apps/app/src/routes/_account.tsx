import { createFileRoute, Outlet } from "@tanstack/react-router";

import { AppShell } from "@/components/shell/app-shell";
import { requireSession } from "@/lib/auth";

const AccountLayout = () => (
  <AppShell>
    <Outlet />
  </AppShell>
);

/** The signed-in pages outside any workspace: the list of workspaces and the account. */
export const Route = createFileRoute("/_account")({
  beforeLoad: (route) => ({ session: requireSession(route) }),
  component: AccountLayout,
});
