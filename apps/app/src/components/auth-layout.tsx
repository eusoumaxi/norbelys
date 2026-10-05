import { MARK, markPath } from "@brand/geometry";
import type { CSSProperties, ReactNode } from "react";

import { Brand } from "@/components/brand";
import { DOCS } from "@/lib/links";

import "./auth-layout.css";

const MARK_PATH = markPath();

/** The @ drawn large and faint on the brand's pink: drawn once, then travelled by a brighter dash. */
const PinkMark = ({ className }: { className?: string }) => (
  <svg
    aria-hidden
    className={className ? `auth-mark ${className}` : "auth-mark"}
    fill="none"
    viewBox="0 0 64 64"
  >
    <path
      className="auth-mark-line"
      d={MARK_PATH}
      pathLength={1}
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={MARK.stroke}
    />
    <path
      className="auth-mark-comet"
      d={MARK_PATH}
      pathLength={1}
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={MARK.stroke}
    />
  </svg>
);

/** The replies that land in the panel, one after another. The people and companies are made up. */
const REPLIES = [
  {
    company: "Northwind",
    initials: "AM",
    message: "Love this. Are you free Tuesday at 10?",
    name: "Alex Morgan",
    tag: "Interested",
  },
  {
    company: "Lumen Labs",
    initials: "PS",
    message: "Send over the deck. Looks relevant for Q3.",
    name: "Priya Shah",
    tag: "Interested",
  },
  {
    company: "Solano Foods",
    initials: "ML",
    message: "Looping in our Head of Sales. She owns this.",
    name: "María López",
    tag: "Referral",
  },
  {
    company: "Kraftwerk",
    initials: "JW",
    message: "Not this quarter. Try me again in January?",
    name: "Jonas Weber",
    tag: "Not now",
  },
] as const;

/**
 * The other half of these pages on wide screens: the brand's pink, its @, replies landing in a
 * square inbox and the promise underneath.
 */
const BrandPanel = () => (
  <aside className="auth-pink relative isolate hidden min-h-dvh flex-col justify-between gap-12 overflow-hidden px-[clamp(48px,7vw,104px)] py-[clamp(40px,8vh,80px)] lg:flex">
    <PinkMark />
    <div aria-hidden className="auth-feed my-auto">
      {REPLIES.map((reply, index) => (
        <div
          className="auth-reply"
          key={reply.name}
          style={{ "--i": index } as CSSProperties}
        >
          <span className="auth-reply-avatar">{reply.initials}</span>
          <span className="flex min-w-0 flex-1 flex-col">
            <span className="flex items-start justify-between gap-3">
              <span className="truncate text-[14px] leading-5 font-semibold">
                {reply.name}
                <span className="font-normal text-[#6d6e6f]"> replied</span>
              </span>
              <span className="auth-reply-tag">{reply.tag}</span>
            </span>
            <span className="truncate text-xs text-[#6d6e6f]">
              Re: Quick idea for {reply.company}
            </span>
            <span className="mt-2 truncate text-[15px] leading-5">
              {reply.message}
            </span>
          </span>
        </div>
      ))}
    </div>
    <div className="relative max-w-[540px]">
      <p className="font-display text-[clamp(40px,3.7vw,58px)] leading-[1.02] font-semibold tracking-[-0.04em]">
        Cold email that gets replies.
      </p>
      <p className="mt-4 max-w-[430px] text-[16px] leading-[26px] text-white/85">
        Send from your own inboxes, follow up on autopilot and stop the second
        someone replies.
      </p>
    </div>
  </aside>
);

/**
 * The pages a person reaches before the app: sign-in, a sign-in link's landing, a device's or an
 * application's approval, an invitation. White, in the app's square corners: a title in the
 * brand's display face, one or two sentences that explain the step, the form and a footer. Wide
 * screens show the brand panel beside it; narrow ones open on a band of the same pink.
 */
export const AuthLayout = ({
  children,
  footer,
  subtitle,
  title,
}: {
  children: ReactNode;
  footer?: ReactNode;
  subtitle?: ReactNode;
  title: ReactNode;
}) => (
  <div className="auth-page light bg-surface text-fg grid min-h-dvh w-full lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)]">
    <div className="flex min-h-dvh flex-col">
      <header className="auth-band relative isolate flex items-start justify-between overflow-hidden px-6 pt-6 max-lg:h-36 max-lg:bg-[#db2777] sm:px-10 lg:items-center lg:pt-8">
        <PinkMark className="lg:hidden" />
        <a
          aria-label="Norbelys home"
          className="max-lg:[&_.nb-word]:text-white max-lg:[&_.text-accent]:text-white"
          href="https://norbelys.com"
        >
          <Brand className="h-7" />
        </a>
        <a
          className="text-fg-3 hover:text-fg text-[13px] leading-7 transition-colors max-lg:text-white/85 max-lg:hover:text-white"
          href={DOCS}
          rel="noreferrer"
          target="_blank"
        >
          Docs
        </a>
      </header>
      <main className="flex flex-1 flex-col justify-center px-6 py-10 sm:px-10 lg:py-14">
        <div className="mx-auto flex w-full max-w-[400px] flex-col">
          <h1 className="font-display text-[38px] leading-[42px] font-semibold tracking-[-0.035em] sm:text-[44px] sm:leading-[48px]">
            {title}
          </h1>
          {subtitle ? (
            <div className="text-fg-2 mt-3 text-[15px] leading-6">
              {subtitle}
            </div>
          ) : null}
          <div className="mt-9 flex flex-col">{children}</div>
          {footer ? (
            <div className="text-fg-3 mt-8 text-[13px] leading-5">{footer}</div>
          ) : null}
        </div>
      </main>
      <footer className="text-fg-3 flex flex-wrap items-center gap-x-5 gap-y-1 px-6 pb-6 text-xs sm:px-10">
        <span>© {new Date().getFullYear()} Norbelys</span>
        <a
          className="hover:text-fg transition-colors"
          href="https://norbelys.com"
        >
          norbelys.com
        </a>
      </footer>
    </div>
    <BrandPanel />
  </div>
);

/** `—— or ——`: two hairlines and a 12px caption between them. */
export const AuthDivider = ({ children }: { children: ReactNode }) => (
  <div className="my-6 flex items-center">
    <hr className="border-line flex-1 border-t" />
    <span className="text-fg-3 px-3 text-xs leading-5">{children}</span>
    <hr className="border-line flex-1 border-t" />
  </div>
);
