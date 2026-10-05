import { queryOptions, useQueryClient } from "@tanstack/react-query";
import { redirect, useRouteContext, useRouter } from "@tanstack/react-router";

import { fetchMe, Session } from "@/lib/session";
import type { Me } from "@/lib/session";

/** The signed-in person (`GET /v1/me`), or `null`. Fresh for a minute; sign-in and sign-out reset it. */
export const meQuery = queryOptions({
  queryKey: ["me"],
  queryFn: ({ signal }) => fetchMe(signal),
  staleTime: 60_000,
});

let current: Session | undefined;

/**
 * One `Session` per browser session: its workspace tokens survive a refreshed `me` (a new
 * membership, a renamed workspace), and a different session id starts over.
 */
export const sessionFor = (me: Me | null): Session | null => {
  if (!me) {
    current = undefined;
    return null;
  }
  if (current?.me.session_id === me.session_id) {
    current.me = me;
    return current;
  }
  current = new Session(me);
  return current;
};

/**
 * Reads the person again (after signing in, or a change to their workspaces or profile) and runs
 * the routes' guards with it. Invalidating alone is not enough: no component observes `me`, the
 * root route only ensures it, so a stale `null` would keep a fresh session out.
 */
export const useRefreshMe = () => {
  const queryClient = useQueryClient();
  const router = useRouter();
  return async () => {
    await queryClient.fetchQuery({ ...meQuery, staleTime: 0 });
    await router.invalidate();
  };
};

/** The signed-in session; only under the routes that require one. */
export const useSession = (): Session =>
  useRouteContext({
    from: "__root__",
    select: (context) => {
      if (!context.session) {
        throw new Error("useSession() needs a signed-in route.");
      }
      return context.session;
    },
  });

/**
 * The guard of a page that needs a signed-in person, for its `beforeLoad`: the session, or a
 * redirect to sign in that comes back to the page.
 */
export const requireSession = ({
  context,
  location,
}: {
  context: { session: Session | null };
  location: { href: string };
}): Session => {
  if (!context.session) {
    throw redirect({ search: { redirect: location.href }, to: "/sign-in" });
  }
  return context.session;
};

/** A path inside the dashboard to return to after signing in; anything else is ignored. */
export const safeReturn = (value: unknown): string | undefined =>
  typeof value === "string" && value.startsWith("/") && !value.startsWith("//")
    ? value
    : undefined;

/**
 * Signs the person out of this browser: the API ends the session (an already ended one is fine),
 * every cached answer is forgotten, and the browser lands on sign-in.
 */
export const useSignOut = () => {
  const session = useSession();
  const queryClient = useQueryClient();
  const router = useRouter();
  return async () => {
    try {
      await session.signOut();
    } catch {
      // Already ended elsewhere: this browser forgets it all the same.
    }
    queryClient.clear();
    queryClient.setQueryData(meQuery.queryKey, null);
    sessionFor(null);
    await router.navigate({ to: "/sign-in" });
  };
};
