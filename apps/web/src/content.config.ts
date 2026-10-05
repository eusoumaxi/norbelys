import { glob } from "astro/loaders";
import { z } from "astro/zod";
import { defineCollection } from "astro:content";

/**
 * The blog: one Markdown file per post in `src/content/blog`, whose name is the post's address
 * (`/blog/<name>`). Each post names its cover, one of the site's own icons on one of four
 * grounds, so the blog is illustrated by the same family as the rest of the site. Drafts are
 * left out of every page and feed.
 */
const blog = defineCollection({
  loader: glob({ base: "./src/content/blog", pattern: "**/*.md" }),
  schema: z.object({
    author: z.string().default("The Norbelys team"),
    cover: z.object({
      glyph: z.enum([
        "ai",
        "broadcasts",
        "campaigns",
        "connect",
        "inbox",
        "mail",
        "meetings",
        "tracking",
        "warmup",
      ]),
      tone: z.enum(["pink", "blush", "ink", "mist"]),
    }),
    date: z.coerce.date(),
    description: z.string(),
    draft: z.boolean().default(false),
    featured: z.boolean().default(false),
    tag: z.enum(["Product", "Playbooks", "Deliverability", "Templates"]),
    title: z.string(),
    updated: z.coerce.date().optional(),
  }),
});

/**
 * The legal documents: one Markdown file per document in `src/content/legal`, whose name is its
 * address (`/legal/<name>`). Each `##` heading starts a clause and a quote straight under it is
 * the clause's plain-words note (src/clauses.ts). Company details that may change are written as
 * placeholders, `{{address}}` or `{{regions}}`, and filled in from src/company.ts. `revisions` is
 * the document's record, newest first: the first entry is the version on the page, and each entry
 * may show what changed.
 */
const legal = defineCollection({
  loader: glob({ base: "./src/content/legal", pattern: "**/*.md" }),
  schema: z.object({
    /** Who to write to about this document: an inbox from src/company.ts. */
    contact: z.enum(["legal", "privacy", "abuse"]),
    description: z.string(),
    /** Where the document sits in the binder. */
    order: z.number().int(),
    revisions: z
      .array(
        z.object({
          changes: z
            .array(
              z.object({
                added: z.string().optional(),
                removed: z.string().optional(),
                section: z.string(),
              })
            )
            .default([]),
          effective: z.coerce.date(),
          summary: z.string(),
          version: z.string(),
        })
      )
      .min(1),
    /** One plain sentence under the title: what the document is for. */
    summary: z.string(),
    /** The document's name on its binder tab. */
    tab: z.string(),
    title: z.string(),
  }),
});

export const collections = { blog, legal };
