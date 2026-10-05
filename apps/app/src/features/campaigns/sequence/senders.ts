import type { CampaignObject } from "@norbelys/sdk";

import type { SenderIdentity } from "@/features/campaigns/queries";

/** Whether an identity can send now: enabled, on a mailbox that works and is not paused. */
export const working = ({ connection, identity }: SenderIdentity): boolean =>
  identity.enabled && connection.status === "active" && !connection.paused;

/** Whether an identity is in the campaign's pool: named, or carrying one of the pool's tags. */
const inPool = (campaign: CampaignObject, { identity }: SenderIdentity) =>
  campaign.senders.identity_ids.includes(identity.id) ||
  identity.tags.some((tag) => campaign.senders.tags.includes(tag));

/** The first identity of the campaign's pool that can send now, or `null` when none can. */
export const poolSender = (
  campaign: CampaignObject,
  senders: readonly SenderIdentity[]
): SenderIdentity | null =>
  senders.find((sender) => inPool(campaign, sender) && working(sender)) ?? null;

/**
 * Who a test or a preview of the campaign is sent from by default: the first identity of its pool
 * that can send now, else the first of the workspace's mailboxes that can, else `null`.
 */
export const defaultSender = (
  campaign: CampaignObject,
  senders: readonly SenderIdentity[]
): SenderIdentity | null =>
  poolSender(campaign, senders) ?? senders.find(working) ?? null;
