import { ArrowLeft01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { createFileRoute, Link } from "@tanstack/react-router";

import { Brand } from "@/components/brand";
import { Illustration } from "@/components/illustration";
import { CreateWorkspace } from "@/features/workspaces/create-workspace";
import { requireSession, useSession, useSignOut } from "@/lib/auth";
import { homeWorkspace } from "@/lib/workspace";

/**
 * Creating a workspace, on a page of its own with nothing else around it: the first thing a new
 * account sees after its code, and where "New workspace" leads. One question (the name), a
 * drawing of what the workspace is for, and a way back for someone who already has one.
 */
const NewWorkspace = () => {
  const session = useSession();
  const signOut = useSignOut();
  const back = homeWorkspace(session.memberships.map((m) => m.workspace.slug));
  const first = back === null;
  return (
    <div className="dark bg-surface text-fg page-glow flex min-h-dvh flex-col">
      <header className="flex items-center justify-between gap-4 px-6 py-5 sm:px-10">
        <Brand className="h-6" />
        <div className="text-fg-3 flex min-w-0 items-center gap-3 text-sm">
          <span className="hidden truncate sm:inline">{session.me.email}</span>
          <button
            className="text-fg-2 hover:text-fg cursor-pointer transition-colors"
            onClick={() => {
              void signOut();
            }}
            type="button"
          >
            Log out
          </button>
        </div>
      </header>
      <main className="flex flex-1 flex-col items-center justify-center px-6 pb-16">
        <div className="flex w-full max-w-[420px] flex-col">
          {back ? (
            <Link
              className="text-fg-3 hover:text-fg mb-8 flex w-fit items-center gap-1.5 text-sm transition-colors"
              params={{ slug: back }}
              to="/w/$slug"
            >
              <HugeiconsIcon className="size-4" icon={ArrowLeft01Icon} />
              Back to your workspace
            </Link>
          ) : null}
          <Illustration className="-ml-3 w-[260px]" name="welcome" />
          <h1 className="font-display text-fg mt-6 text-[32px] leading-10 font-semibold tracking-[-0.02em]">
            {first ? "Let's set up your workspace" : "Create a workspace"}
          </h1>
          <p className="text-fg-2 mt-3 mb-8 text-sm leading-[22px]">
            {first
              ? "Your mailboxes, the people you write to and every reply live in a workspace. Name it after your company or team."
              : "A separate home for another company or client, with its own mailboxes, people, campaigns and keys."}
          </p>
          <CreateWorkspace />
          {first ? (
            <p className="text-fg-3 mt-6 text-xs leading-[18px]">
              Next, we&apos;ll walk you through connecting a mailbox, adding the
              people you want to reach and launching your first campaign.
            </p>
          ) : null}
        </div>
      </main>
    </div>
  );
};

export const Route = createFileRoute("/new")({
  beforeLoad: ({ context, location }) => {
    requireSession({ context, location });
  },
  head: () => ({ meta: [{ title: "Create a workspace · Norbelys" }] }),
  component: NewWorkspace,
});
