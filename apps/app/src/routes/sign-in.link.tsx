import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useState, useSyncExternalStore } from "react";

import { AuthLayout } from "@/components/auth-layout";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { useRefreshMe } from "@/lib/auth";
import { describeProblem } from "@/lib/problem";
import { auth, isSignedOut } from "@/lib/session";

const subscribeFragment = (onChange: () => void): (() => void) => {
  window.addEventListener("hashchange", onChange);
  window.addEventListener("popstate", onChange);
  return () => {
    window.removeEventListener("hashchange", onChange);
    window.removeEventListener("popstate", onChange);
  };
};
const currentFragment = (): string => window.location.hash;
const serverFragment = (): string => "";

/** The token and the address a sign-in mail's link carries in its fragment (never sent to a server). */
const readFragment = (
  fragment: string
): { email: string; token: string } | null => {
  const params = new URLSearchParams(fragment.slice(1));
  const token = params.get("token");
  const email = params.get("email");
  return token && email ? { email, token } : null;
};

/**
 * Where a sign-in mail's link lands, in any browser: the page names the account and signs in only
 * when the person confirms, so a mail scanner opening the link signs nobody in.
 */
const SignInLink = ({ fragment }: { fragment: string }) => {
  const link = readFragment(fragment);
  const navigate = useNavigate();
  const refreshMe = useRefreshMe();
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [refused, setRefused] = useState(false);

  if (!link) {
    return (
      <AuthLayout
        subtitle="This link is incomplete. Open it again from the email, or ask for a new code."
        title="Link not valid"
      >
        <Button
          className="h-12 w-full text-[15px]"
          render={<Link to="/sign-in" />}
          variant="secondary"
        >
          Back to sign in
        </Button>
      </AuthLayout>
    );
  }

  return (
    <AuthLayout
      subtitle={
        <>
          Sign in as <span className="text-fg font-semibold">{link.email}</span>
          ?
        </>
      }
      title="Sign in to Norbelys"
    >
      <div className="flex flex-col gap-3">
        {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
        {refused ? (
          <Button
            className="h-12 w-full text-[15px]"
            render={<Link to="/sign-in" />}
            variant="primary"
          >
            Request a new sign-in email
          </Button>
        ) : (
          <Button
            className="h-12 w-full text-[15px]"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              setProblem(null);
              try {
                await auth.finishLink(link.token, link.email);
                window.history.replaceState(null, "", window.location.pathname);
                await refreshMe();
                await navigate({ to: "/" });
              } catch (error) {
                setProblem(describeProblem(error).detail);
                setRefused(isSignedOut(error));
                setBusy(false);
              }
            }}
            variant="primary"
          >
            {busy ? <Spinner /> : null}
            Continue as {link.email}
          </Button>
        )}
        <Button
          className="h-12 w-full text-[15px]"
          render={<Link to="/sign-in" />}
          variant="tertiary"
        >
          Use another account
        </Button>
      </div>
    </AuthLayout>
  );
};

/** A new email may open this same document with only a different fragment. */
const SignInLinkRoute = () => {
  const fragment = useSyncExternalStore(
    subscribeFragment,
    currentFragment,
    serverFragment
  );
  return <SignInLink fragment={fragment} key={fragment} />;
};

export const Route = createFileRoute("/sign-in/link")({
  head: () => ({ meta: [{ title: "Sign in · Norbelys" }] }),
  component: SignInLinkRoute,
});
