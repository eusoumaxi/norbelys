import type { CampaignObject, CreateCampaign, Norbelys } from "@norbelys/sdk";
import { z } from "zod";

export const NEWSLETTER_PREFIX = "[Newsletter] ";

const editionSchema = z.object({
  id: z.string().regex(/^[a-z0-9-]{1,48}$/u),
  title: z.string().min(1).max(100),
  category: z.enum(["education", "marketing", "product", "promotion"]),
  audience: z.string().min(1).max(140),
  subject: z.string().min(1).max(200),
  preheader: z.string().max(1000),
  html: z
    .string()
    .min(1)
    .max(256 * 1024),
  source: z.url().refine((url) => new URL(url).protocol === "https:"),
  proposed_at: z.iso.datetime({ offset: true }).nullable(),
});

export const packSchema = z
  .object({
    version: z.literal(1),
    id: z.string().regex(/^[a-z0-9-]{1,48}$/u),
    name: z.string().min(1).max(100),
    timezone: z.string().refine((zone) => {
      try {
        return Boolean(
          new Intl.DateTimeFormat("en", { timeZone: zone }).resolvedOptions()
            .timeZone
        );
      } catch {
        return false;
      }
    }, "Use an IANA time zone"),
    editions: z.array(editionSchema).min(1).max(50),
  })
  .superRefine((pack, context) => {
    if (
      new Set(pack.editions.map((edition) => edition.id)).size !==
      pack.editions.length
    ) {
      context.addIssue({
        code: "custom",
        message: "Edition IDs must be unique",
      });
    }
  });

export type NewsletterPack = z.infer<typeof packSchema>;
export type Edition = NewsletterPack["editions"][number];

export const editionName = (pack: NewsletterPack, edition: Edition): string =>
  `${NEWSLETTER_PREFIX}${pack.id}/${edition.id} · ${edition.title}`;

/** One independent broadcast, with no follow-up, enrollment or activation side effect. */
export const draftBody = (
  pack: NewsletterPack,
  edition: Edition,
  senderId: string
): CreateCampaign => ({
  name: editionName(pack, edition),
  schedule: { timezone: pack.timezone, start_at: edition.proposed_at },
  senders: { identity_ids: [senderId], tags: [] },
  tracking: { opens: true, clicks: true },
  stop_rules: { on_reply: "campaign" },
  steps: [
    {
      name: `${edition.category} · ${edition.audience}`,
      delay_seconds: 0,
      same_thread: false,
      variants: [
        {
          name: "Newsletter",
          subject: edition.subject,
          preheader: edition.preheader,
          html: edition.html,
        },
      ],
    },
  ],
});

/** Stable across a timed-out request and a page reload; changed content is a different request. */
export const draftKey = async (
  workspaceId: string,
  body: CreateCampaign
): Promise<string> => {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(JSON.stringify(body))
  );
  const hex = [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `newsletter:${workspaceId}:${hex}`;
};

/** Re-import resumes a partial pack and preserves drafts already edited by a person. */
export const prepareDrafts = async (
  api: Norbelys,
  workspaceId: string,
  pack: NewsletterPack,
  senderId: string,
  onProgress: (completed: number) => void
): Promise<void> => {
  const existing = new Set<string>();
  for await (const campaign of api.campaigns.list({
    q: `${NEWSLETTER_PREFIX}${pack.id}/`,
    limit: 100,
  })) {
    existing.add(campaign.name);
  }
  for (const [index, edition] of pack.editions.entries()) {
    const name = editionName(pack, edition);
    if (!existing.has(name)) {
      const body = draftBody(pack, edition, senderId);
      // oxlint-disable-next-line no-await-in-loop -- sequential writes bound load and preserve progress on failure
      const idempotencyKey = await draftKey(workspaceId, body);
      // oxlint-disable-next-line no-await-in-loop -- a pack may be resumed after any individual draft
      await api.campaigns.create(body, { idempotencyKey });
      existing.add(name);
    }
    onProgress(index + 1);
  }
};

export const readPack = async (file: File): Promise<NewsletterPack> => {
  if (file.size > 4 * 1024 * 1024) {
    throw new Error("Choose a newsletter pack smaller than 4 MiB.");
  }
  const value: unknown = JSON.parse(await file.text());
  return packSchema.parse(value);
};

export const loadNewsletters = async (
  api: Norbelys,
  signal: AbortSignal
): Promise<CampaignObject[]> => {
  const rows: CampaignObject[] = [];
  for await (const campaign of api.campaigns.list(
    { q: NEWSLETTER_PREFIX, limit: 100 },
    { signal }
  )) {
    rows.push(campaign);
  }
  return rows;
};
