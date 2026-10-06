import { MailAccount01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ConnectionObject } from "@norbelys/sdk";
import { cn } from "cn";
import { useEffect, useState } from "react";

import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import {
  allowance,
  kindLabel,
  providerInfo,
  warmupShare,
} from "@/features/mailboxes/providers";
import { formatCount, formatTimestamp } from "@/lib/format";
import { statusTone } from "@/lib/status";

/** How the sender paces a connection: one campaign email every few minutes, or by its limits. */
export const paceText = (connection: ConnectionObject): string =>
  connection.send_interval_minutes
    ? `One campaign email every ${formatCount(connection.send_interval_minutes)} minutes`
    : "As fast as its limits allow";

/** Where a connection's warm-up stands: off, or the share of its daily limit it may use for now. */
export const warmupText = (connection: ConnectionObject): string => {
  const stage = connection.warmup_stage;
  return stage === null || stage === undefined
    ? "Off"
    : `${warmupShare(stage)}% of the daily limit for now, rising each clean day`;
};

/** How often the clock that ends holds and pauses on screen moves, in milliseconds. */
const CLOCK_MS = 60_000;

/** The time now, read again every minute, so a hold or a pause that ends shows as ended. */
export const useNow = (): number => {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), CLOCK_MS);
    return () => clearInterval(timer);
  }, []);
  return now;
};

/**
 * Until when a hold lasts (the breaker's `paused_until` on a connection, a provider's on a quota
 * scope), when it lasts beyond `now`; the API keeps an ended hold's time.
 */
export const holdUntil = (
  pausedUntil: string | null | undefined,
  now: number
): string | null =>
  pausedUntil && Date.parse(pausedUntil) > now ? pausedUntil : null;

/** Managed addresses distinguish provisioning from an established transport. */
const managedStatus = (status: string): string => {
  switch (status) {
    case "active": {
      return "Ready";
    }
    case "verifying": {
      return "Setting up";
    }
    case "archived": {
      return "Disconnected";
    }
    default: {
      return "Needs attention";
    }
  }
};

/**
 * A connection's health as a dot and a word. A working connection that is not sending says why
 * instead of "Active": Paused (a person paused it) or Waiting (its provider refused several
 * emails in a row, so the sender holds it for a while). Any other state is the API's, with
 * Paused or Waiting beside it as tags.
 */
export const ConnectionHealth = ({
  connection,
}: {
  connection: ConnectionObject;
}) => {
  const held = holdUntil(connection.paused_until, useNow());
  const waiting = held
    ? `Waiting until ${formatTimestamp(held)}: the provider refused several emails in a row`
    : undefined;
  if (connection.status === "active" && (connection.paused || held)) {
    return (
      <span className="flex flex-wrap items-center gap-1.5">
        <Badge
          dot
          title={connection.paused ? undefined : waiting}
          tone="warning"
        >
          {connection.paused ? "Paused" : "Waiting"}
        </Badge>
        {connection.paused && held ? (
          <Badge title={waiting} tone="warning">
            Waiting
          </Badge>
        ) : null}
      </span>
    );
  }
  return (
    <span className="flex flex-wrap items-center gap-1.5">
      {connection.provider === "norbelys" ? (
        <Badge dot tone={statusTone("connection", connection.status)}>
          {managedStatus(connection.status)}
        </Badge>
      ) : (
        <StatusBadge kind="connection" value={connection.status} />
      )}
      {connection.paused ? <Badge tone="warning">Paused</Badge> : null}
      {held ? (
        <Badge title={waiting} tone="warning">
          Waiting
        </Badge>
      ) : null}
    </span>
  );
};

/** Whether a relay's webhook key is saved: its delivery reports are refused until it is. */
export const WebhookKeyBadge = ({ set }: { set: boolean }) =>
  set ? (
    <Badge dot tone="success">
      Key saved
    </Badge>
  ) : (
    <Badge dot tone="warning">
      No key yet
    </Badge>
  );

/** What a connection may send today: its daily limit, or the share of it warm-up allows. */
const todayLimit = (connection: ConnectionObject): number =>
  allowance(connection.daily_limit, connection.warmup_stage);

/**
 * Today's use of what the connection may send today: "12 of 50", after a thin bar that fills as
 * it is used. A new figure slides the bar in 200 ms; the first one is drawn as it is.
 */
export const TodayMeter = ({
  className,
  connection,
}: {
  className?: string;
  connection: ConnectionObject;
}) => {
  const { used } = connection.usage.today;
  const limit = todayLimit(connection);
  const share = limit > 0 ? Math.min(used / limit, 1) : 0;
  const words = `${formatCount(used)} of ${formatCount(limit)}`;
  return (
    <span className={cn("flex items-center gap-2.5", className)}>
      {/* The bar draws the words beside it, which are what a screen reader reads. */}
      <span
        aria-hidden
        className="bg-hover h-1.5 w-16 shrink-0 overflow-hidden rounded-full"
      >
        <span
          className="bg-chart block h-full rounded-full transition-[width] duration-200 ease-(--nb-ease-out) motion-reduce:transition-none"
          style={{ width: `${share * 100}%` }}
        />
      </span>
      <span className="text-fg whitespace-nowrap tabular-nums">{words}</span>
    </span>
  );
};

/** A mailbox in a list: its provider's mark, its address, and what kind of account it is. */
export const MailboxCell = ({
  connection,
}: {
  connection: ConnectionObject;
}) => (
  <span className="flex min-w-0 items-center gap-3">
    <HugeiconsIcon
      className="text-icon size-4 shrink-0"
      icon={providerInfo(connection.provider)?.icon ?? MailAccount01Icon}
    />
    <span className="flex min-w-0 flex-col">
      <span className="text-fg truncate font-bold">
        {connection.account.email}
      </span>
      <span className="text-fg-3 truncate text-xs">
        {kindLabel(connection.provider)}
      </span>
    </span>
  </span>
);
