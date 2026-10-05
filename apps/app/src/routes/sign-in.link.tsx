import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";

import { AuthLayout } from "@/components/auth-layout";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { useRefreshMe } from "@/lib/auth";
import { describeProblem } from "@/lib/problem";
import { auth } from "@/lib/session";

/** The token and the address a sign-in mail's link carries in its fragment (never sent to a server). */
const readFragment = (): { email: string; token: string } | null => {
  const params = new URLSearchParams(window.location.hash.slice(1));
  const token = params.get("token");
  const email = params.get("email");
  return token && email ? { email, token } : null;
};

/**
 * Where a sign-in mail's link lands, in any browser: the page names the account and signs in only
 * when the person confirms, so a mail scanner opening the link signs nobody in.
 */
const SignInLink = () => {
  const link = useMemo(() => readFragment(), []);
  const navigate = useNavigate();
  const refreshMe = useRefreshMe();
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  if (!link) {
    return (
      <AuthLayout
        subtitle="This link is incomplete. Open it again from the email, or ask for a new code."
        title="Link not valid"
      >
        <Button render={<Link to="/sign-in" />} variant="secondary">
          Back to log in
        </Button>
      </AuthLayout>
    );
  }

  return (
    <AuthLayout
      subtitle={
        <>
          Sign in as <span className="text-fg">{link.email}</span>?
        </>
      }
      title="Log in to Norbelys"
    >
      <div className="flex flex-col gap-6">
        {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
        <Button
          className="w-full"
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
              setBusy(false);
            }
          }}
          variant="primary"
        >
          {busy ? <Spinner /> : null}
          Continue as {link.email}
        </Button>
        <Button render={<Link to="/sign-in" />} variant="tertiary">
          Use another account
        </Button>
      </div>
    </AuthLayout>
  );
};

export const Route = createFileRoute("/sign-in/link")({
  head: () => ({ meta: [{ title: "Log in · Norbelys" }] }),
  component: SignInLink,
});
