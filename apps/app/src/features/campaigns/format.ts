import type { CampaignObject, OnSenderRemoved } from "@norbelys/sdk";

import { formatWindow, plural, zoneName } from "@/lib/format";

/** `2 days, 3 hours`, `90 minutes`: a wait in its largest whole units. */
export const formatDelay = (seconds: number): string => {
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const parts = [
    days > 0 ? plural(days, "day") : null,
    hours > 0 ? plural(hours, "hour") : null,
    minutes > 0 ? plural(minutes, "minute") : null,
  ].filter((part) => part !== null);
  return parts.length > 0 ? parts.join(", ") : "No wait";
};

export const ON_SENDER_REMOVED: Record<OnSenderRemoved, string> = {
  reassign: "Another address continues",
  stop: "That conversation stops",
};

/** `3 emails · Mon–Fri, 09:00–17:00, Madrid time`: how long a sequence is and when it sends. */
export const planLine = (campaign: CampaignObject): string => {
  const steps =
    campaign.steps.length === 0
      ? "No emails yet"
      : plural(campaign.steps.length, "email");
  return `${steps} · ${formatWindow(campaign.schedule.send_window)}, ${zoneName(campaign.schedule.timezone)}`;
};

/** A route match as a page's `head` sees it: which route, and what its loader returned. */
interface HeadMatch {
  routeId: string;
  loaderData?: unknown;
}

/**
 * The browser tab of a campaign's section: `Enrollments · Founders outreach · Norbelys`, the
 * campaign's name taken from its page's loader, so several open tabs tell themselves apart.
 */
export const campaignSectionHead = (
  matches: readonly HeadMatch[],
  section: string
) => {
  const data = matches.find(
    (match) => match.routeId === "/w/$slug/campaigns/$campaignId"
  )?.loaderData;
  const name =
    typeof data === "object" &&
    data !== null &&
    "name" in data &&
    typeof data.name === "string"
      ? data.name
      : null;
  return {
    meta: [
      {
        title: name
          ? `${section} · ${name} · Norbelys`
          : `${section} · Norbelys`,
      },
    ],
  };
};
