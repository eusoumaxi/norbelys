import { QueryClient } from "@tanstack/react-query";
import { createRouter } from "@tanstack/react-router";

import { PageLoader } from "@/components/brand";
import { NotFound } from "@/components/not-found";
import { RouteError } from "@/components/problem";
import { meQuery } from "@/lib/auth";
import { isSignedOut } from "@/lib/session";
import { reportBrowserError } from "@/lib/telemetry";
import { routeTree } from "@/routeTree.gen";

export const queryClient = new QueryClient({
  defaultOptions: {
    // The SDK already retries what is safe to repeat; retrying here would multiply it.
    queries: { retry: false, staleTime: 30_000 },
  },
});

export const router = createRouter({
  routeTree,
  context: { queryClient, session: null },
  defaultErrorComponent: RouteError,
  defaultOnCatch: () => reportBrowserError("route_error"),
  defaultNotFoundComponent: NotFound,
  // A route's skeleton (pendingComponent) shows only when its loader takes longer than this,
  // then stays at least 500 ms (the default) so it never flickers.
  defaultPendingMs: 300,
  defaultPendingComponent: PageLoader,
  defaultPreload: "intent",
  // Loaders go through TanStack Query, which decides what is still fresh.
  defaultPreloadStaleTime: 0,
  scrollRestoration: true,
});

// A request answered 401 means the session ended (signed out elsewhere, revoked, expired): forget
// the person and let the guards send the browser to sign in.
const signedOut = (error: unknown) => {
  if (isSignedOut(error)) {
    queryClient.setQueryData(meQuery.queryKey, null);
    void router.invalidate();
  }
};
/** Cache libraries permit arbitrary rejection payloads; authentication inspects unknown. */
const cacheChanged = (event: {
  type: string;
  action?: { type: string; error?: unknown };
}) => {
  if (event.type === "updated" && event.action?.type === "error") {
    signedOut(event.action.error);
  }
};
queryClient.getQueryCache().subscribe(cacheChanged);
queryClient.getMutationCache().subscribe(cacheChanged);

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
