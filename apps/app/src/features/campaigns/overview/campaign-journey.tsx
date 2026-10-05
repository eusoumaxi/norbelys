import { ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { CampaignObject, EnrollmentSummary } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import type { ReactNode } from "react";

import { formatDelay } from "@/features/campaigns/format";
import { poolOf } from "@/features/campaigns/pool";
import { sendersQuery } from "@/features/campaigns/queries";
import { formatCount, plural, windowPhrase, zoneName } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** A count of people; nothing at all while the API does not count them for this answer. */
const Count = ({
  accent = false,
  value,
}: {
  accent?: boolean;
  value: number | null;
}) => {
  if (value === null) {
    return null;
  }
  return (
    <span
      className={cn(
        "tabular-nums",
        value === 0 && "text-fg-3",
        value > 0 && (accent ? "text-accent" : "text-fg")
      )}
    >
      {formatCount(value)}
    </span>
  );
};

/** One stop of the journey: how many people are there, and what it is. */
const Station = ({
  caption,
  count,
  label,
}: {
  caption?: ReactNode;
  count: ReactNode;
  label: ReactNode;
}) => (
  <div className="flex min-w-[112px] shrink-0 flex-col gap-1">
    {count ? (
      <span className="text-3xl leading-8 font-semibold">{count}</span>
    ) : null}
    <span className="text-fg max-w-[180px] truncate text-sm font-medium">
      {label}
    </span>
    {caption ? <span className="text-fg-3 text-xs">{caption}</span> : null}
  </div>
);

/** The arrow between two stops, level with their names. */
const Arrow = ({ counted }: { counted: boolean }) => (
  <HugeiconsIcon
    aria-hidden
    className={cn(
      "text-fg-4 mx-4 size-4 shrink-0",
      counted ? "mt-9.5" : "mt-0.5"
    )}
    icon={ArrowRight01Icon}
  />
);

/** `Email 2 · 2 days later`: which email a stop is, and how long after the one before. */
const emailCaption = (index: number, seconds: number) => {
  if (index === 0) {
    return "Email 1";
  }
  const wait =
    seconds > 0
      ? `${formatDelay(seconds).split(", ")[0]} later`
      : "right after";
  return `Email ${formatCount(index + 1)} · ${wait}`;
};

/** How the campaign sends, in one line: when, from whom, and what a reply does. */
const Plan = ({ campaign }: { campaign: CampaignObject }) => {
  const workspace = useWorkspace();
  const senders = useQuery(sendersQuery(workspace));
  const pool = senders.data ? poolOf(campaign.senders, senders.data) : null;
  const { schedule, stop_rules: stop } = campaign;
  const reply = {
    all: "A reply stops every campaign for that person",
    campaign: "A reply stops it for that person",
    none: "Replies don't stop it",
  }[stop.on_reply];
  const parts = [
    `Sends ${windowPhrase(schedule.send_window)}, ${zoneName(schedule.timezone)}`,
    pool ? `from ${plural(pool.length, "address", "addresses")}` : null,
    reply,
  ].filter((part) => part !== null);
  return (
    <p className="text-fg-3 flex flex-wrap items-center gap-x-1.5 text-xs">
      <span>{parts.join(" · ")}</span>
      <Link
        className="text-fg-2 hover:text-fg underline-offset-2 transition-colors hover:underline"
        params={{ campaignId: campaign.id, slug: workspace.slug }}
        to="/w/$slug/campaigns/$campaignId/settings"
      >
        Change
      </Link>
    </p>
  );
};

/**
 * Where everyone is now: each email of the sequence with the people waiting for it, the waits
 * between them, then how people left it (replied, in pink: what matters; finished; stopped). It
 * reads left to right like the sequence itself and scrolls sideways when the sequence is long.
 * Without counts from the API it still shows the sequence's shape.
 */
export const CampaignJourney = ({
  campaign,
  summary,
}: {
  campaign: CampaignObject;
  summary: EnrollmentSummary | null;
}) => {
  const waiting = (stepId: string) =>
    summary
      ? (summary.steps.find((step) => step.step_id === stepId)?.live ?? 0)
      : null;
  const workspace = useWorkspace();
  const stopped = summary ? summary.stopped + summary.failed : null;
  return (
    <section className="flex flex-col gap-4">
      <h2 className="text-fg text-xl font-semibold">Where everyone is</h2>
      {campaign.steps.length === 0 ? (
        <p className="text-fg-2 text-sm">
          The sequence has no email yet.{" "}
          <Link
            className="text-fg underline underline-offset-2"
            params={{ campaignId: campaign.id, slug: workspace.slug }}
            to="/w/$slug/campaigns/$campaignId/sequence"
          >
            Write the first one
          </Link>
          .
        </p>
      ) : (
        <div className="border-line flex scrollbar-thin items-start overflow-x-auto rounded-sm border px-6 py-5">
          <ol className="flex items-start">
            {campaign.steps.map((step, index) => (
              <li className="flex items-start" key={step.id}>
                {index > 0 ? <Arrow counted={summary !== null} /> : null}
                <Station
                  caption={emailCaption(index, step.delay_seconds)}
                  count={summary ? <Count value={waiting(step.id)} /> : null}
                  label={step.name}
                />
              </li>
            ))}
          </ol>
          <div
            aria-hidden
            className="bg-line mx-6 w-px shrink-0 self-stretch"
          />
          <div className="flex items-start gap-8">
            <Station
              caption="Their conversation is in the inbox"
              count={summary ? <Count accent value={summary.replied} /> : null}
              label="Replied"
            />
            <Station
              caption="Got every email"
              count={summary ? <Count value={summary.completed} /> : null}
              label="Finished"
            />
            {stopped !== null && stopped > 0 ? (
              <Station
                caption="Stopped early"
                count={summary ? <Count value={stopped} /> : null}
                label="Stopped"
              />
            ) : null}
          </div>
        </div>
      )}
      <Plan campaign={campaign} />
    </section>
  );
};
