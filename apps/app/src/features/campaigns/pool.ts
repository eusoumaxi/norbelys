import type { SenderIdentity } from "@/features/campaigns/queries";
import { formatAddress } from "@/lib/format";

/** The parts of a campaign's pool that decide who is in it. */
interface PoolRule {
  identity_ids: readonly string[];
  tags: readonly string[];
}

/**
 * The identities a pool holds now, as the API reads it when it assigns a sender: the ones the
 * campaign names, plus every enabled identity carrying one of its tags.
 */
export const poolOf = (
  rule: PoolRule,
  senders: readonly SenderIdentity[]
): SenderIdentity[] =>
  senders.filter(
    ({ identity }) =>
      rule.identity_ids.includes(identity.id) ||
      (identity.enabled && identity.tags.some((tag) => rule.tags.includes(tag)))
  );

/** `Ana <ana@acme.com>`, or the address alone. */
export const identityLabel = ({ identity }: SenderIdentity): string =>
  formatAddress(identity);
