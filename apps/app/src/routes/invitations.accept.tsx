import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";

import { AuthLayout } from "@/components/auth-layout";
import { ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { requireSession, useRefreshMe, useSession } from "@/lib/auth";
import { describeProblem } from "@/lib/problem";

/** The token an invitation mail's link carries in its fragment. */
const readToken = (): string | null =>
  new URLSearchParams(window.location.hash.slice(1)).get("token");

/**
 * Where an invitation mail's link lands: the signed-in person joins the workspace, provided they
 * are signed in with the address the invitation was sent to.
 */
const AcceptInvitation = () => {
  const session = useSession();
  const refreshMe = useRefreshMe();
  const navigate = useNavigate();
  const token = useMemo(() => readToken(), []);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  if (!token) {
    return (
      <AuthLayout
        subtitle="This link is incomplete. Open it again from the invitation email."
        title="Invitation not valid"
      >
        <span />
      </AuthLayout>
    );
  }

  const accept = async () => {
    setBusy(true);
    setProblem(null);
    try {
      const membership = await session.acceptInvitation(token);
      window.history.replaceState(null, "", window.location.pathname);
      await refreshMe();
      await navigate({
        params: { slug: membership.workspace.slug },
        to: "/w/$slug",
      });
    } catch (error) {
      setProblem(describeProblem(error).detail);
      setBusy(false);
    }
  };

  return (
    <AuthLayout
      subtitle={
        <>
          Join the workspace as{" "}
          <span className="text-fg">{session.me.email}</span>. The invitation
          must have been sent to this address.
        </>
      }
      title="Accept invitation"
    >
      <div className="flex flex-col gap-6">
        {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
        <Button
          disabled={busy}
          onClick={() => {
            void accept();
          }}
          variant="primary"
        >
          {busy ? <Spinner /> : null}
          Join workspace
        </Button>
      </div>
    </AuthLayout>
  );
};

export const Route = createFileRoute("/invitations/accept")({
  beforeLoad: (route) => {
    requireSession(route);
  },
  head: () => ({ meta: [{ title: "Accept invitation · Norbelys" }] }),
  component: AcceptInvitation,
});
