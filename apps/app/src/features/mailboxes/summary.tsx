import type { ConnectionObject } from "@norbelys/sdk";
import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { Section } from "@/components/page";
import { Button } from "@/components/ui/button";
import { paceText, TodayMeter, warmupText } from "@/features/mailboxes/parts";
import { providerInfo, providerLabel } from "@/features/mailboxes/providers";
import {
  formatCount,
  formatRelative,
  formatWindow,
  zoneName,
} from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** One line of the summary: what it is about, then what holds, in a sentence. */
const Row = ({ children, label }: { children: ReactNode; label: string }) => (
  <div className="grid gap-x-6 gap-y-1 px-5 py-3 sm:grid-cols-[120px_minmax(0,1fr)]">
    <dt className="text-fg-3">{label}</dt>
    <dd className="text-fg min-w-0 break-words">{children}</dd>
  </div>
);

/** Who takes the mailbox's emails from Norbelys, as the line under today's count names it. */
const handedTo = (connection: ConnectionObject): string =>
  providerInfo(connection.provider)?.way === "login"
    ? "its mail server"
    : providerLabel(connection.provider);

/** Today's count against what it may send today, with yesterday's and what is on its way now. */
const Today = ({ connection }: { connection: ConnectionObject }) => {
  const { today, yesterday } = connection.usage;
  const beside = [
    `${formatCount(yesterday.used)} yesterday`,
    today.reserved > 0 ? `${formatCount(today.reserved)} on the way now` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <div className="flex flex-col gap-1">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <TodayMeter connection={connection} />
        <span className="text-fg-3 text-xs">{beside}</span>
      </div>
      <p className="text-fg-3 text-xs">
        Emails handed to {handedTo(connection)} since midnight UTC, delivered or
        not.
      </p>
    </div>
  );
};

/** Why a connection reads no folder, and what to do about it. */
const notRead = (connection: ConnectionObject): string => {
  const way = providerInfo(connection.provider)?.way;
  if (way === "login") {
    return "Not read: add its IMAP server in the settings to see replies here.";
  }
  if (way === "oauth") {
    return "Not read: choose its folders in the settings to see replies here.";
  }
  return "Not read: a relay only sends.";
};

/** Which folders are read for replies, when they were last read, and any that keeps failing. */
const Replies = ({ connection }: { connection: ConnectionObject }) => {
  const folders = connection.receiving.folders.filter(
    (folder) => folder.enabled
  );
  if (folders.length === 0) {
    return <span className="text-fg-2">{notRead(connection)}</span>;
  }
  const last = folders
    .map((folder) => folder.polled_at)
    .filter((polled): polled is string => Boolean(polled))
    .toSorted()
    .at(-1);
  const failing = folders.filter((folder) => folder.failures > 0);
  return (
    <div className="flex flex-col gap-1">
      <span>
        Read from {folders.map((folder) => folder.folder).join(", ")}
        <span className="text-fg-3">
          {" · "}
          {last
            ? `last read ${formatRelative(last).toLowerCase()}`
            : "not read yet"}
        </span>
      </span>
      {failing.map((folder) => (
        <span className="text-warning text-xs" key={folder.id}>
          {folder.folder}: {formatCount(folder.failures)}{" "}
          {folder.failures === 1 ? "read" : "reads"} failed in a row
          {folder.status_detail ? `: ${folder.status_detail}` : "."}
        </span>
      ))}
    </div>
  );
};

/**
 * How a mailbox sends, at a glance and in plain sentences: today's count against what it may
 * send today, its pace, when it sends, its warm-up, and whether its replies are read. Changing
 * any of it is one click away, in the settings.
 */
export const SendingSummary = ({
  connection,
}: {
  connection: ConnectionObject;
}) => {
  const workspace = useWorkspace();
  return (
    <Section
      actions={
        <Button
          nativeButton={false}
          render={
            <Link
              params={{ connectionId: connection.id, slug: workspace.slug }}
              to="/w/$slug/mailboxes/$connectionId/settings"
            />
          }
          size="s"
          variant="secondary"
        >
          Change settings
        </Button>
      }
      title="Sending"
    >
      <dl className="border-line divide-line divide-y rounded-sm border">
        <Row label="Today">
          <Today connection={connection} />
        </Row>
        <Row label="Pace">{paceText(connection)}</Row>
        <Row label="When">
          {formatWindow(connection.send_window)}
          <span className="text-fg-3"> · {zoneName(connection.timezone)}</span>
        </Row>
        <Row label="Warm-up">{warmupText(connection)}</Row>
        <Row label="Replies">
          <Replies connection={connection} />
        </Row>
      </dl>
    </Section>
  );
};
