import type { AttemptObject, HoldObject, MessageObject } from "@norbelys/sdk";

import { Copyable } from "@/components/copy";
import { DataTable, Dash } from "@/components/data-table";
import { StatusBadge } from "@/components/status-badge";
import { RelativeTime } from "@/components/time";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { formatRelative, formatTimestamp, humanize } from "@/lib/format";

/** Where an attempt ended, as SMTP names its commands. */
const PHASES: Record<string, string> = {
  api: "API",
  auth: "AUTH",
  connect: "Connect",
  data: "DATA",
  mail_from: "MAIL FROM",
  rcpt_to: "RCPT TO",
};

/** An attempt's outcome; one still running has none yet. */
const Outcome = ({ attempt }: { attempt: AttemptObject }) => {
  if (!attempt.outcome) {
    return (
      <Badge dot tone="info">
        Running
      </Badge>
    );
  }
  return <StatusBadge kind="attempt" value={attempt.outcome} />;
};

/** What the server or provider said: its codes, where it ended, its words and its message id. */
const Diagnostics = ({ attempt }: { attempt: AttemptObject }) => {
  const codes = [
    attempt.smtp_code === null || attempt.smtp_code === undefined
      ? null
      : String(attempt.smtp_code),
    attempt.enhanced_status,
    attempt.phase ? `at ${PHASES[attempt.phase] ?? attempt.phase}` : null,
    attempt.category ? humanize(attempt.category) : null,
  ].filter(Boolean);
  if (
    codes.length === 0 &&
    !attempt.diagnostic &&
    !attempt.provider_message_id
  ) {
    return <Dash />;
  }
  return (
    <div className="flex max-w-[520px] min-w-0 flex-col gap-0.5 py-1">
      {codes.length > 0 ? (
        <span className="text-fg font-mono text-xs">{codes.join(" · ")}</span>
      ) : null}
      {attempt.diagnostic ? (
        <span className="text-fg-2 text-xs break-words whitespace-normal">
          {attempt.diagnostic}
        </span>
      ) : null}
      {attempt.provider_message_id ? (
        <span className="text-fg-3 text-xs">
          Provider ID <Copyable mono value={attempt.provider_message_id} />
        </span>
      ) : null}
    </div>
  );
};

/**
 * The message's latest attempts (at most 20, newest first): when each started, how it ended and
 * what the receiving side said. Older ones are read through an export of attempts.
 */
export const AttemptsCard = ({ message }: { message: MessageObject }) => (
  <Card>
    <CardHeader className="flex-col items-start gap-0.5">
      <CardTitle>Attempts</CardTitle>
      <CardDescription className="text-xs">
        {message.attempts.has_more
          ? `The latest ${message.attempts.data.length} of ${message.attempts_count}; export the attempts for the others.`
          : "Each time a sender handed the message to its server or provider."}
      </CardDescription>
    </CardHeader>
    {message.attempts.data.length === 0 ? (
      <p className="text-fg-3 border-line border-t px-4 py-6 text-sm">
        No attempt yet: the message waits for its time and its mailbox’s pacing.
      </p>
    ) : (
      <DataTable<AttemptObject>
        columns={[
          {
            render: (attempt) => (
              <span className="text-fg font-semibold tabular-nums">
                {attempt.number}
              </span>
            ),
            className: "w-12",
            header: "#",
            id: "number",
          },
          {
            render: (attempt) => (
              <RelativeTime
                className="text-fg-2 whitespace-nowrap"
                value={attempt.started_at ?? attempt.claimed_at}
              />
            ),
            header: "Started",
            id: "started",
          },
          {
            render: (attempt) => <Outcome attempt={attempt} />,
            header: "Outcome",
            id: "outcome",
          },
          {
            render: (attempt) => <Diagnostics attempt={attempt} />,
            header: "Diagnostics",
            id: "diagnostics",
          },
        ]}
        rowKey={(attempt) => attempt.id}
        rows={message.attempts.data}
      />
    )}
  </Card>
);

const HOLD_REASONS: Record<string, string> = {
  greylisted: "Greylisted",
  invalid_recipient: "Reported invalid, waiting for review",
  mailbox_full: "Mailbox full",
  no_route: "No mail route",
};

/** One held recipient: why, since when, and either when it is checked again or how it ended. */
const HoldRow = ({ hold }: { hold: HoldObject }) => (
  <li className="border-line flex flex-wrap items-start justify-between gap-x-4 gap-y-1 border-b px-4 py-3 last:border-b-0">
    <div className="flex min-w-0 flex-col gap-0.5">
      <span className="text-fg font-mono text-xs break-all">{hold.email}</span>
      <span className="text-fg-2 text-sm">
        {HOLD_REASONS[hold.reason] ?? humanize(hold.reason)}
      </span>
      <span className="text-fg-3 text-xs">
        Observed {formatTimestamp(hold.observed_at)}
        {hold.resolved_at
          ? ` · resolved ${formatTimestamp(hold.resolved_at)}`
          : ` · checked again ${formatRelative(hold.review_after).toLowerCase()}`}
      </span>
    </div>
    {hold.resolved_at ? (
      <Badge tone="muted">
        {hold.resolution ? humanize(hold.resolution) : "Resolved"}
      </Badge>
    ) : (
      <Badge dot tone="warning">
        Held
      </Badge>
    )}
  </li>
);

/**
 * The recipients this message's evidence held: mail to a held address waits until a later success
 * lifts the hold, its time runs out, or a person releases it.
 */
export const HoldsCard = ({ holds }: { holds: readonly HoldObject[] }) => (
  <Card>
    <CardHeader className="flex-col items-start gap-0.5">
      <CardTitle>Holds</CardTitle>
      <CardDescription className="text-xs">
        Mail to a held address waits: a later success lifts the hold, or its
        time runs out, or you release it.
      </CardDescription>
    </CardHeader>
    <CardContent className="px-0 pb-0">
      <ul className="border-line border-t">
        {holds.map((hold) => (
          <HoldRow hold={hold} key={`${hold.email}-${hold.observed_at}`} />
        ))}
      </ul>
    </CardContent>
  </Card>
);
