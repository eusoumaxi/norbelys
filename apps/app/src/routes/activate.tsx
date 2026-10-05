import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";

import { AuthLayout } from "@/components/auth-layout";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Consent } from "@/features/oauth/consent";
import { requireSession } from "@/lib/auth";

/** Where a person types the code their terminal shows, when it did not open this page with it. */
const EnterCode = () => {
  const navigate = useNavigate();
  const [code, setCode] = useState("");
  return (
    <AuthLayout
      subtitle="Enter the code shown by `norbelys login` or another device."
      title="Connect a device"
    >
      <form
        className="flex flex-col gap-6"
        onSubmit={(event) => {
          event.preventDefault();
          void navigate({
            search: { user_code: code.trim() },
            to: "/activate",
          });
        }}
      >
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="user-code">Code</Label>
          <Input
            autoComplete="one-time-code"
            autoFocus
            className="text-center font-mono text-base tracking-[0.3em] uppercase"
            id="user-code"
            onChange={(event) => setCode(event.target.value)}
            placeholder="ABCD-EFGH"
            value={code}
          />
        </div>
        <Button
          disabled={code.trim().length < 4}
          type="submit"
          variant="secondary"
        >
          Continue
        </Button>
      </form>
    </AuthLayout>
  );
};

const Activate = () => {
  const { user_code: userCode } = Route.useSearch();
  return userCode ? (
    <Consent key={userCode} userCode={userCode} />
  ) : (
    <EnterCode />
  );
};

/**
 * The device approval page of the OAuth device flow (`norbelys login`): the person signs in, checks
 * the code their terminal shows, picks the workspace and approves.
 */
export const Route = createFileRoute("/activate")({
  validateSearch: z.object({ user_code: z.string().optional() }),
  beforeLoad: (route) => {
    requireSession(route);
  },
  head: () => ({ meta: [{ title: "Connect a device · Norbelys" }] }),
  component: Activate,
});
