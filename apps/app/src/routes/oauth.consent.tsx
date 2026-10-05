import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";

import { AuthLayout } from "@/components/auth-layout";
import { Consent } from "@/features/oauth/consent";
import { requireSession } from "@/lib/auth";

const ConsentPage = () => {
  const { request } = Route.useSearch();
  if (!request) {
    return (
      <AuthLayout
        subtitle="This page opens from an app asking for access; start again from that app."
        title="Nothing to approve"
      >
        <span />
      </AuthLayout>
    );
  }
  return <Consent request={request} />;
};

/**
 * Where `/oauth/authorize` sends the browser: an MCP client (or another registered app) asks to act
 * in one of the person's workspaces, and the person allows or denies it.
 */
export const Route = createFileRoute("/oauth/consent")({
  validateSearch: z.object({ request: z.string().optional() }),
  beforeLoad: (route) => {
    requireSession(route);
  },
  head: () => ({ meta: [{ title: "Allow access · Norbelys" }] }),
  component: ConsentPage,
});
