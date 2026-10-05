import { ArrowRight01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Link } from "@tanstack/react-router";
import type { LinkProps } from "@tanstack/react-router";
import { cn } from "cn";
import type { ReactNode } from "react";

import { Illustration } from "@/components/illustration";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";

/** One line of a short list: what it is, a quieter second line, something on the right. */
export interface ListRow {
  key: string;
  link: LinkProps;
  primary: ReactNode;
  secondary?: ReactNode;
  trailing?: ReactNode;
}

const LOADING_ROWS = ["a", "b", "c"];

/**
 * A card with the first few items of a list (the workspace's mailboxes, campaigns, conversations),
 * each opening its page, and a link to the whole list; one sentence and an action when it is empty.
 */
export const ListCard = ({
  all,
  empty,
  loading,
  rows,
  title,
}: {
  all: LinkProps;
  empty: ReactNode;
  loading: boolean;
  rows: ListRow[];
  title: string;
}) => (
  <Card className="min-w-0">
    <CardHeader className="flex-row items-center justify-between">
      <CardTitle>{title}</CardTitle>
      <Link
        {...all}
        className="text-fg-3 hover:text-fg flex items-center gap-1 text-xs font-semibold transition-colors"
      >
        View all
        <HugeiconsIcon className="size-3.5" icon={ArrowRight01Icon} />
      </Link>
    </CardHeader>
    <CardContent className="pt-0">
      {loading ? (
        <ul className="flex flex-col gap-3 py-1">
          {LOADING_ROWS.map((row) => (
            <li key={row}>
              <Skeleton className="h-4 w-full" />
            </li>
          ))}
        </ul>
      ) : null}
      {!loading && rows.length === 0 ? (
        <div className="text-fg-3 flex min-h-24 flex-col items-start justify-center gap-3 text-sm">
          {empty}
        </div>
      ) : null}
      {!loading && rows.length > 0 ? (
        <ul className="-mx-2 flex flex-col">
          {rows.map((row) => (
            <li key={row.key}>
              <Link
                {...row.link}
                className="hover:bg-hover flex min-h-11 items-center gap-3 rounded-sm px-2 py-1.5 transition-colors"
              >
                <span className="flex min-w-0 flex-1 flex-col">
                  <span className="text-fg truncate text-sm font-medium">
                    {row.primary}
                  </span>
                  {row.secondary ? (
                    <span className="text-fg-3 truncate text-xs">
                      {row.secondary}
                    </span>
                  ) : null}
                </span>
                {row.trailing}
              </Link>
            </li>
          ))}
        </ul>
      ) : null}
    </CardContent>
  </Card>
);

/** One step of setting a workspace up: done or not, what it is, and the way to do it. */
export interface SetupStep {
  action: SetupAction;
  description: string;
  done: boolean;
  title: string;
}

/** Where a setup step's button leads, and what it says. */
interface SetupAction {
  label: string;
  link: LinkProps;
}

/**
 * What a new workspace still needs before it can send, as a short checklist: each step ticked off
 * as the workspace gets it. Only the next step to do carries a button (pink, the setup's next
 * move); the ones after it wait their turn, so there is never a question of where to start. The
 * drawing beside it draws itself on the first visit only.
 */
export const GettingStarted = ({ steps }: { steps: SetupStep[] }) => {
  const done = steps.filter((step) => step.done).length;
  const next = steps.findIndex((step) => !step.done);
  return (
    <Card className="min-w-0 lg:flex-row lg:items-center">
      <div className="flex min-w-0 flex-1 flex-col">
        <CardHeader className="flex-row items-start justify-between gap-4">
          <div className="flex flex-col gap-0.5">
            <CardTitle>Get ready to send</CardTitle>
            <CardDescription>
              Three steps, a few minutes. Nothing is sent until you start a
              campaign.
            </CardDescription>
          </div>
          <span className="text-fg-3 shrink-0 text-xs tabular-nums">
            {done} of {steps.length} done
          </span>
        </CardHeader>
        <CardContent className="pt-0">
          <ol className="flex flex-col">
            {steps.map((step, index) => (
              <li
                className="border-line flex items-center gap-3 border-t py-3 first:border-t-0"
                key={step.title}
              >
                <span
                  className={cn(
                    "grid size-6 shrink-0 place-items-center rounded-full border text-xs font-semibold tabular-nums",
                    step.done
                      ? "border-accent bg-accent text-surface"
                      : "border-line-strong text-fg-2",
                    index === next && "border-accent text-accent"
                  )}
                >
                  {step.done ? (
                    <HugeiconsIcon className="size-3.5" icon={Tick02Icon} />
                  ) : (
                    index + 1
                  )}
                </span>
                <span className="flex min-w-0 flex-1 flex-col">
                  <span
                    className={cn(
                      "text-sm font-medium",
                      step.done ? "text-fg-3" : "text-fg"
                    )}
                  >
                    {step.title}
                  </span>
                  {step.done ? null : (
                    <span className="text-fg-3 text-xs">
                      {step.description}
                    </span>
                  )}
                </span>
                {index === next ? (
                  <Button
                    render={<Link {...step.action.link} />}
                    size="s"
                    variant="connect"
                  >
                    {step.action.label}
                  </Button>
                ) : null}
              </li>
            ))}
          </ol>
        </CardContent>
      </div>
      <Illustration
        className="hidden w-[280px] shrink-0 self-center px-6 lg:block"
        name="welcome"
        once="nb.drawn.welcome"
      />
    </Card>
  );
};
