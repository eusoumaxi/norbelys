import {
  ArrowLeft01Icon,
  FingerPrintIcon,
  GithubIcon,
  GoogleIcon,
  MicrosoftIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { IconSvgElement } from "@hugeicons/react";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { z } from "zod";

import { AuthDivider, AuthLayout } from "@/components/auth-layout";
import { ProblemAlert } from "@/components/problem";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { safeReturn, useRefreshMe } from "@/lib/auth";
import { problemLine } from "@/lib/problem";
import { auth } from "@/lib/session";
import type { Challenge } from "@/lib/session";
import { getPasskey, passkeysSupported } from "@/lib/webauthn";

const authConfigQuery = {
  queryFn: ({ signal }: { signal: AbortSignal }) => auth.config(signal),
  queryKey: ["auth", "config"],
  staleTime: 5 * 60_000,
};

/** The identity providers the dashboard knows how to show; others get their name. */
const PROVIDERS: Record<string, { icon: IconSvgElement; label: string }> = {
  github: { icon: GithubIcon, label: "GitHub" },
  google: { icon: GoogleIcon, label: "Google" },
  microsoft: { icon: MicrosoftIcon, label: "Microsoft" },
};

/** How long before another code can be asked for, in seconds. */
const RESEND_AFTER = 30;

/** The seconds left before `seconds` have passed since `since` (a timestamp; 0 when unset). */
const useCountdown = (seconds: number, since: number) => {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  if (since === 0) {
    return 0;
  }
  return Math.min(
    seconds,
    Math.max(0, seconds - Math.floor((now - since) / 1000))
  );
};

/** Follows a challenge that names another page (single sign-on, an identity provider). */
const follow = (started: Challenge) => {
  if (started.authorization_url) {
    window.location.assign(started.authorization_url);
    return true;
  }
  return false;
};

type Busy = "email" | "code" | "passkey" | "resend" | `provider:${string}`;

/**
 * Sign-in, which is also sign-up: an email code (the code proves the inbox, and the first one
 * creates the account), a passkey, or an identity provider when the deployment offers one. A
 * workspace that enforces single sign-on answers the email step with its provider's page.
 */
const SignIn = () => {
  const { redirect: back } = Route.useSearch();
  const navigate = useNavigate();
  const refreshMe = useRefreshMe();
  const config = useQuery(authConfigQuery);
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [challenge, setChallenge] = useState<Challenge | null>(null);
  const [sentAt, setSentAt] = useState(0);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const resendIn = useCountdown(RESEND_AFTER, sentAt);

  const signedIn = async () => {
    await refreshMe();
    await navigate({ href: safeReturn(back) ?? "/" });
  };

  const sendCode = async (again = false) => {
    setBusy(again ? "resend" : "email");
    setProblem(null);
    try {
      const started = await auth.startEmail(email.trim());
      if (follow(started)) {
        return;
      }
      setChallenge(started);
      setSentAt(Date.now());
      setCode("");
    } catch (error) {
      setProblem(problemLine(error));
    }
    setBusy(null);
  };

  const verify = async (value: string) => {
    if (!challenge) {
      return;
    }
    setBusy("code");
    setProblem(null);
    try {
      await auth.finishCode(challenge.id, value);
      await signedIn();
    } catch (error) {
      setProblem(problemLine(error));
      setBusy(null);
    }
  };

  const passkey = async () => {
    setBusy("passkey");
    setProblem(null);
    try {
      const started = await auth.startPasskey();
      const credential = await getPasskey(started.options);
      if (credential) {
        await auth.finishPasskey(started.id, credential);
        await signedIn();
        return;
      }
    } catch (error) {
      setProblem(
        error instanceof DOMException
          ? "This browser or device could not use a passkey for Norbelys."
          : problemLine(error)
      );
    }
    setBusy(null);
  };

  const provider = async (name: string) => {
    setBusy(`provider:${name}`);
    setProblem(null);
    try {
      if (follow(await auth.startProvider(name, safeReturn(back) ?? "/"))) {
        return;
      }
    } catch (error) {
      setProblem(problemLine(error));
    }
    setBusy(null);
  };

  const methods = config.data?.methods ?? ["email_code"];
  const providers = config.data?.oidc_providers ?? [];
  const offersPasskey = methods.includes("passkey") && passkeysSupported();
  const captchaRequired = Boolean(config.data?.captcha);
  const validEmail = /^\S+@\S+\.\S+$/u.test(email.trim());

  if (challenge) {
    return (
      <AuthLayout
        footer={
          <button
            className="text-fg-2 hover:text-fg inline-flex cursor-pointer items-center gap-1.5 transition-colors"
            onClick={() => {
              setChallenge(null);
              setSentAt(0);
              setProblem(null);
            }}
            type="button"
          >
            <HugeiconsIcon className="size-4" icon={ArrowLeft01Icon} />
            Use a different email
          </button>
        }
        subtitle={
          <>
            We sent a 6-digit code to{" "}
            <span className="text-fg font-medium">{email.trim()}</span>. Enter
            it here; it works for 10 minutes, in this browser.
          </>
        }
        title="Check your inbox"
      >
        <form
          className="flex flex-col gap-5"
          onSubmit={(event) => {
            event.preventDefault();
            void verify(code);
          }}
        >
          <div className="flex flex-col gap-2">
            <Label htmlFor="code">Sign-in code</Label>
            <Input
              autoComplete="one-time-code"
              autoFocus
              className="h-14 text-center font-mono text-2xl tracking-[0.5em]"
              id="code"
              inputMode="numeric"
              maxLength={6}
              onChange={(event) => {
                const digits = event.target.value.replaceAll(/\D/gu, "");
                setCode(digits);
                if (digits.length === 6 && busy === null) {
                  void verify(digits);
                }
              }}
              pattern="[0-9]{6}"
              placeholder="······"
              value={code}
            />
          </div>
          {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
          <Button
            className="h-10 w-full"
            disabled={code.length !== 6 || busy !== null}
            type="submit"
            variant="primary"
          >
            {busy === "code" ? <Spinner /> : null}
            Sign in
          </Button>
        </form>
        <div className="border-line bg-chrome text-fg-3 mt-6 rounded-md border px-4 py-3 text-xs leading-[18px]">
          <p>
            The subject reads{" "}
            <span className="text-fg-2">
              &ldquo;…is your Norbelys sign-in code&rdquo;
            </span>
            . Opened it on your phone? The link inside signs you in there.
            Nothing yet? Check spam, or{" "}
            {resendIn > 0 ? (
              <span>ask for a new code in {resendIn}s.</span>
            ) : (
              <button
                className="text-link hover:text-link-hover cursor-pointer"
                disabled={busy !== null}
                onClick={() => {
                  void sendCode(true);
                }}
                type="button"
              >
                send a new code.
              </button>
            )}
          </p>
        </div>
      </AuthLayout>
    );
  }

  return (
    <AuthLayout
      footer={
        <>
          <span className="text-fg-2">New to Norbelys?</span> Sign in with your
          email: the first code creates your account, with no password and no
          separate sign-up. Your workspaces come next.
        </>
      }
      subtitle="Use your work email and we'll send you a 6-digit code."
      title="Sign in to Norbelys"
    >
      {providers.length > 0 ? (
        <>
          <div className="flex flex-col gap-2">
            {providers.map((name) => {
              const known = PROVIDERS[name];
              return (
                <Button
                  className="h-10 w-full"
                  disabled={busy !== null}
                  key={name}
                  onClick={() => {
                    void provider(name);
                  }}
                  variant="secondary"
                >
                  {busy === `provider:${name}` ? (
                    <Spinner />
                  ) : (
                    known && <HugeiconsIcon icon={known.icon} />
                  )}
                  Continue with {known?.label ?? name}
                </Button>
              );
            })}
          </div>
          <AuthDivider>or</AuthDivider>
        </>
      ) : null}
      <form
        className="flex flex-col gap-5"
        onSubmit={(event) => {
          event.preventDefault();
          void sendCode();
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor="email">Work email</Label>
          <Input
            autoComplete="email webauthn"
            autoFocus
            className="h-10"
            id="email"
            onChange={(event) => setEmail(event.target.value)}
            placeholder="ada@company.com"
            required
            type="email"
            value={email}
          />
        </div>
        {captchaRequired ? (
          <Alert variant="warning">
            <AlertDescription className="col-span-2 col-start-1">
              This deployment asks for a captcha, which this dashboard does not
              show yet.
            </AlertDescription>
          </Alert>
        ) : null}
        {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
        <Button
          className="h-10 w-full"
          disabled={!validEmail || busy !== null}
          type="submit"
          variant="primary"
        >
          {busy === "email" ? <Spinner /> : null}
          Continue with email
        </Button>
      </form>
      {offersPasskey ? (
        <Button
          className="mt-3 h-10 w-full"
          disabled={busy !== null}
          onClick={() => {
            void passkey();
          }}
          variant="secondary"
        >
          {busy === "passkey" ? (
            <Spinner />
          ) : (
            <HugeiconsIcon icon={FingerPrintIcon} />
          )}
          Sign in with a passkey
        </Button>
      ) : null}
    </AuthLayout>
  );
};

export const Route = createFileRoute("/sign-in/")({
  validateSearch: z.object({ redirect: z.string().optional() }),
  beforeLoad: ({ context, search }) => {
    if (context.session) {
      throw redirect({ href: safeReturn(search.redirect) ?? "/" });
    }
  },
  head: () => ({ meta: [{ title: "Sign in · Norbelys" }] }),
  component: SignIn,
});
