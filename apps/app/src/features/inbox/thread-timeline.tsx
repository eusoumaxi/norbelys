import { MailReceive01Icon, MailSend01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ThreadMessage, ThreadObject } from "@norbelys/sdk";
import { Link } from "@tanstack/react-router";

import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { formatTimestamp } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** The page of one entry: the message we sent, or the message the inbox read. */
const EntryLink = ({ entry }: { entry: ThreadMessage }) => {
  const workspace = useWorkspace();
  const className =
    "text-link hover:text-link-hover shrink-0 text-xs font-semibold";
  if (entry.direction === "outbound") {
    return (
      <Link
        className={className}
        params={{ messageId: entry.id, slug: workspace.slug }}
        to="/w/$slug/messages/$messageId"
      >
        Delivery details
      </Link>
    );
  }
  return (
    <Link
      className={className}
      params={{ inboundId: entry.id, slug: workspace.slug }}
      to="/w/$slug/inbox/received/$inboundId"
    >
      Classification and evidence
    </Link>
  );
};

/**
 * One message of the conversation: who wrote it and to whom, when, its state (ours) or its
 * classification (theirs), its subject when it differs from the thread's, and the start of its
 * text.
 */
const Entry = ({
  entry,
  threadSubject,
}: {
  entry: ThreadMessage;
  threadSubject: string | null | undefined;
}) => {
  const outbound = entry.direction === "outbound";
  return (
    <li className="border-line bg-surface rounded-sm border">
      <div className="flex items-start gap-3 px-4 py-3">
        <span
          aria-hidden
          className="bg-chrome flex size-7 shrink-0 items-center justify-center rounded-sm"
        >
          <HugeiconsIcon
            className={outbound ? "text-icon size-4" : "text-accent size-4"}
            icon={outbound ? MailSend01Icon : MailReceive01Icon}
          />
        </span>
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-fg min-w-0 truncate text-sm font-semibold">
              {entry.from ?? "Unknown sender"}
            </span>
            {outbound && entry.state ? (
              <StatusBadge kind="message" value={entry.state} />
            ) : null}
            {!outbound && entry.classification ? (
              <StatusBadge kind="classification" value={entry.classification} />
            ) : null}
          </div>
          <span className="text-fg-3 text-xs">
            {outbound ? `Sent to ${entry.to.join(", ")}` : "Received"}
            {" · "}
            <time dateTime={entry.at}>{formatTimestamp(entry.at)}</time>
          </span>
          {entry.subject && entry.subject !== threadSubject ? (
            <span className="text-fg-2 text-sm">{entry.subject}</span>
          ) : null}
        </div>
        <EntryLink entry={entry} />
      </div>
      {entry.text ? (
        <p className="border-line text-fg-2 border-t px-4 py-3 text-sm break-words whitespace-pre-wrap">
          {entry.text}
        </p>
      ) : null}
    </li>
  );
};

/**
 * The conversation, oldest first: our messages and their answers as one timeline (the latest 50;
 * older ones are reached through the message and received mail lists, narrowed to this thread).
 */
export const ThreadTimeline = ({ thread }: { thread: ThreadObject }) => {
  const workspace = useWorkspace();
  const entries = thread.messages?.data ?? [];
  if (entries.length === 0) {
    return (
      <p className="border-line text-fg-3 rounded-sm border px-4 py-6 text-sm">
        This conversation has no message to show yet.
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-3">
      {thread.messages?.has_more ? (
        <Alert variant="neutral">
          <AlertDescription className="col-span-2 col-start-1">
            Only the latest {entries.length} messages are shown. Older ones:{" "}
            <Link
              params={{ slug: workspace.slug }}
              search={{ thread_id: thread.id }}
              to="/w/$slug/messages"
            >
              messages sent
            </Link>{" "}
            and{" "}
            <Link
              params={{ slug: workspace.slug }}
              search={{ thread_id: thread.id }}
              to="/w/$slug/inbox/received"
            >
              mail received
            </Link>
            .
          </AlertDescription>
        </Alert>
      ) : null}
      <ol aria-label="Conversation" className="flex flex-col gap-3">
        {entries.map((entry) => (
          <Entry entry={entry} key={entry.id} threadSubject={thread.subject} />
        ))}
      </ol>
    </div>
  );
};
