import type { QueryClient } from "@tanstack/react-query";
import {
  createRootRouteWithContext,
  HeadContent,
  Outlet,
} from "@tanstack/react-router";
import { NuqsAdapter } from "nuqs/adapters/tanstack-router";

import { Loader } from "@/components/brand";
import { NotFound } from "@/components/not-found";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { meQuery, sessionFor } from "@/lib/auth";
import type { Session } from "@/lib/session";

interface RouterContext {
  queryClient: QueryClient;
  /** The signed-in session, or null; the root route fills it in before any child loads. */
  session: Session | null;
}

const Root = () => (
  <NuqsAdapter>
    <HeadContent />
    <TooltipProvider>
      <Outlet />
    </TooltipProvider>
    <Toaster position="bottom-right" />
  </NuqsAdapter>
);

/** While the session is read on a first load: black, and the @ drawing itself; never a blank page. */
const Loading = () => (
  <div className="dark bg-surface grid min-h-dvh place-items-center">
    <Loader label="Loading Norbelys" />
  </div>
);

export const Route = createRootRouteWithContext<RouterContext>()({
  beforeLoad: async ({ context }) => {
    const me = await context.queryClient.ensureQueryData(meQuery);
    return { session: sessionFor(me) };
  },
  head: () => ({ meta: [{ title: "Norbelys" }] }),
  component: Root,
  notFoundComponent: NotFound,
  pendingComponent: Loading,
});
