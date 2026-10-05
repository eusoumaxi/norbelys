import {
  ArrowLeft01Icon,
  ArrowRight01Icon,
  FingerPrintIcon,
  GithubIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { useEffect, useState } from "react";
import type { ReactNode } from "react";
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

/** Google's own mark, in its colours, where people expect to see it. */
const GoogleMark = () => (
  <svg aria-hidden viewBox="0 0 48 48">
    <path
      d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"
      fill="#EA4335"
    />
    <path
      d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"
      fill="#4285F4"
    />
    <path
      d="M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z"
      fill="#FBBC05"
    />
    <path
      d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"
      fill="#34A853"
    />
  </svg>
);

/** Microsoft's four squares. */
const MicrosoftMark = () => (
  <svg aria-hidden viewBox="0 0 21 21">
    <path d="M1 1h9v9H1z" fill="#F25022" />
    <path d="M11 1h9v9h-9z" fill="#7FBA00" />
    <path d="M1 11h9v9H1z" fill="#00A4EF" />
    <path d="M11 11h9v9h-9z" fill="#FFB900" />
  </svg>
);

/** The identity providers the dashboard knows how to show; others get their name. */
const PROVIDERS: Record<string, { mark: ReactNode; label: string }> = {
  github: { label: "GitHub", mark: <HugeiconsIcon icon={GithubIcon} /> },
  google: { label: "Google", mark: <GoogleMark /> },
  microsoft: { label: "Microsoft", mark: <MicrosoftMark /> },
};

/** How long before another code can be asked for, in seconds. */
const RESEND_AFTER = 30;

/** A sign-in code's six places. */
const SLOTS = [0, 1, 2, 3, 4, 5] as const;

/** The pink ring a field wears while it has focus, on top of the app's own focus border. */
const FOCUS_RING =
  "focus-visible:border-accent focus-visible:shadow-[0_0_0_3px_var(--nb-go-bg)]";

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

/**
 * The code as six squares over one real input: paste, autofill from the mail and the keyboard
 * all go through the input, and the squares only show what it holds and where the next digit goes.
 */
const CodeSlots = ({
  code,
  onCode,
}: {
  code: string;
  onCode: (digits: string) => void;
}) => {
  const [focused, setFocused] = useState(true);
  const next = Math.min(code.length, SLOTS.length - 1);
  return (
    <div className="relative">
      <div aria-hidden className="grid grid-cols-6 gap-2">
        {SLOTS.map((slot) => {
          const digit = code[slot];
          const current =
            focused && slot === next && code.length < SLOTS.length;
          return (
            <div
              className={cn(
                "border-field-line bg-field text-fg flex h-16 items-center justify-center rounded-sm border font-mono text-[28px] transition-[border-color,box-shadow] duration-200",
                digit === undefined ? "" : "border-fg",
                current
                  ? "border-accent shadow-[0_0_0_3px_var(--nb-go-bg)]"
                  : ""
              )}
              key={slot}
            >
              {digit ?? (current ? <span className="auth-caret" /> : null)}
            </div>
          );
        })}
      </div>
      <input
        autoComplete="one-time-code"
        autoFocus
        className="absolute inset-0 size-full cursor-text bg-transparent text-[16px] text-transparent caret-transparent outline-none selection:bg-transparent"
        id="code"
        inputMode="numeric"
        maxLength={SLOTS.length}
        onBlur={() => setFocused(false)}
        onChange={(event) =>
          onCode(
            event.target.value.replaceAll(/\D/gu, "").slice(0, SLOTS.length)
          )
        }
        onFocus={() => setFocused(true)}
        pattern="[0-9]{6}"
        value={code}
      />
    </div>
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
            <span className="text-fg font-semibold">{email.trim()}</span>. It
            works for 10 minutes, in this browser.
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
            <CodeSlots
              code={code}
              onCode={(digits) => {
                setCode(digits);
                if (digits.length === SLOTS.length && busy === null) {
                  void verify(digits);
                }
              }}
            />
          </div>
          {problem ? <ProblemAlert>{problem}</ProblemAlert> : null}
          <Button
            className="h-12 w-full text-[15px]"
            disabled={code.length !== SLOTS.length || busy !== null}
            type="submit"
            variant="primary"
          >
            {busy === "code" ? <Spinner /> : null}
            Sign in
          </Button>
        </form>
        <p className="text-fg-3 mt-6 text-[13px] leading-5">
          The subject reads{" "}
          <span className="text-fg-2">
            &ldquo;…is your Norbelys sign-in code&rdquo;
          </span>
          . Opened it on your phone? The link inside signs you in there. Nothing
          yet? Check spam, or{" "}
          {resendIn > 0 ? (
            <span>ask for a new code in {resendIn}s.</span>
          ) : (
            <button
              className="text-accent cursor-pointer font-semibold underline-offset-2 hover:underline"
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
      </AuthLayout>
    );
  }

  return (
    <AuthLayout
      footer={
        <>
          <span className="text-fg-2 font-semibold">New to Norbelys?</span> The
          same code creates your account. No password, no separate sign-up.
        </>
      }
      subtitle={
        <>
          Use your work email and we’ll send you a{" "}
          <span className="whitespace-nowrap">6-digit code.</span>
        </>
      }
      title="Sign in to Norbelys"
    >
      {providers.length > 0 ? (
        <>
          <div className="flex flex-col gap-2.5">
            {providers.map((name) => {
              const known = PROVIDERS[name];
              return (
                <Button
                  className="h-12 w-full gap-2.5 text-[15px] [&_svg]:size-[18px]"
                  disabled={busy !== null}
                  key={name}
                  onClick={() => {
                    void provider(name);
                  }}
                  variant="secondary"
                >
                  {busy === `provider:${name}` ? <Spinner /> : known?.mark}
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
            className={cn("h-12 px-3.5 text-[15px]", FOCUS_RING)}
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
          className="group h-12 w-full text-[15px]"
          disabled={!validEmail || busy !== null}
          type="submit"
          variant="primary"
        >
          {busy === "email" ? <Spinner /> : null}
          Continue with email
          {busy === "email" ? null : (
            <HugeiconsIcon
              className="transition-transform duration-300 group-hover:translate-x-1"
              icon={ArrowRight01Icon}
            />
          )}
        </Button>
      </form>
      {offersPasskey ? (
        <Button
          className="mt-3 h-12 w-full text-[15px]"
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
