import type { GlyphName } from "./glyphs";
import { PAGES } from "./links";

/**
 * The product family, as the navigation, the capability list and the footer name it. Products
 * marked `soon` are on the way and are never links.
 */
export interface Product {
  readonly name: string;
  readonly summary: string;
  readonly href?: string;
  readonly soon?: true;
  readonly glyph: GlyphName;
}

export const PRODUCTS: readonly Product[] = [
  {
    glyph: "campaigns",
    href: PAGES.campaigns,
    name: "Campaigns & sequences",
    summary:
      "Multi-step sequences with A/B tests, business-hours sending and smart stops.",
  },
  {
    glyph: "inbox",
    href: "/#one-inbox",
    name: "Unified inbox",
    summary: "Every reply from every mailbox in one place, sorted for you.",
  },
  {
    glyph: "warmup",
    href: "/#deliverability",
    name: "Warm-up & deliverability",
    summary:
      "A gradual ramp, daily limits and list checks that protect your domain.",
  },
  {
    glyph: "ai",
    href: PAGES.aiPersonalization,
    name: "AI personalization",
    summary:
      "A custom first line for every prospect, written only from facts you choose.",
  },
  {
    glyph: "tracking",
    href: "/#pricing",
    name: "Open & click tracking",
    summary: "See who opened and clicked, on your own tracking domain.",
  },
  {
    glyph: "connect",
    href: PAGES.developers,
    name: "MCP & API",
    summary:
      "Run outreach from Claude or any MCP-ready assistant, or wire it into your stack.",
  },
  {
    glyph: "mail",
    name: "Mail",
    soon: true,
    summary: "One-off emails and replies from any connected inbox.",
  },
  {
    glyph: "broadcasts",
    name: "Broadcasts",
    soon: true,
    summary: "Newsletters for the people who said yes.",
  },
  {
    glyph: "meetings",
    name: "Meetings",
    soon: true,
    summary: "Turn “let’s talk” into a booked call.",
  },
];
