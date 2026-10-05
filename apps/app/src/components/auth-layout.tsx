import type { ReactNode } from "react";

import { Brand } from "@/components/brand";
import { Illustration } from "@/components/illustration";
import { DOCS, REPOSITORY } from "@/lib/links";

/**
 * The right half of the pages a person meets before the app, on wide screens: the drawing of what
 * Norbelys does (your mailbox sends, a reply comes back), the promise, and one sentence.
 */
const BrandPanel = () => (
  <aside className="border-line bg-chrome relative hidden overflow-hidden border-l lg:flex lg:flex-col lg:justify-center lg:px-16 xl:px-24">
    <div
      aria-hidden
      className="pointer-events-none absolute inset-0 bg-[radial-gradient(55%_45%_at_70%_30%,rgb(244_114_182/0.12),transparent_70%)]"
    />
    <div className="relative flex max-w-[480px] flex-col">
      <Illustration className="-ml-6 w-full max-w-[460px]" name="welcome" />
      <p className="font-display text-fg mt-10 text-[36px] leading-[42px] font-semibold tracking-[-0.03em]">
        Nothing sends until you approve it.
      </p>
      <p className="text-fg-2 mt-4 text-base leading-[26px]">
        Norbelys sends your outbound from your own mailboxes, at a human pace,
        and tells you what really happened, reply by reply.
      </p>
    </div>
  </aside>
);

/**
 * The pages a person reaches before the app: sign-in, a sign-in link's landing, a device's or an
 * application's approval, an invitation. Always dark. The form column holds the logo, a title in
 * the brand's display face, one or two sentences that explain the step, the form and a footer; on
 * wide screens the brand panel fills the other half.
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
  <div className="dark bg-surface text-fg grid min-h-dvh w-full lg:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)]">
    <div className="flex min-h-dvh flex-col px-6 py-6 sm:px-10">
      <header className="flex items-center justify-between">
        <a aria-label="Norbelys home" href="https://norbelys.com">
          <Brand className="h-6" />
        </a>
        <a
          className="text-fg-3 hover:text-fg text-sm transition-colors"
          href={DOCS}
          rel="noreferrer"
          target="_blank"
        >
          Docs
        </a>
      </header>
      <main className="flex flex-1 flex-col justify-center py-12">
        <div className="mx-auto flex w-full max-w-[380px] flex-col">
          <h1 className="font-display text-fg text-[32px] leading-10 font-semibold tracking-[-0.02em]">
            {title}
          </h1>
          {subtitle ? (
            <div className="text-fg-2 mt-3 text-sm leading-[22px]">
              {subtitle}
            </div>
          ) : null}
          <div className="mt-8 flex flex-col">{children}</div>
          {footer ? (
            <div className="text-fg-3 mt-8 text-sm leading-[22px]">
              {footer}
            </div>
          ) : null}
        </div>
      </main>
      <footer className="text-fg-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
        <span>Open-source outbound email</span>
        <a className="hover:text-fg-2" href={REPOSITORY}>
          GitHub
        </a>
        <a className="hover:text-fg-2" href={DOCS}>
          Documentation
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
