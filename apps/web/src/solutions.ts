import type { GlyphName } from "./glyphs";

/**
 * The teams Norbelys is built for, as the header's Solutions menu, the solutions page and the
 * footer name them. Each team has a page under `/for/` built on its own drawn object (a bell, a
 * framed reply, a stack of reports...), and its icon is that object at icon size.
 */
export interface Team {
  /** The address: `/for/<slug>`. */
  readonly slug: string;
  readonly href: string;
  /** The team, as a sentence names it: "Cold email for {name}". */
  readonly name: string;
  /** The link's label in menus and the footer. */
  readonly label: string;
  /** One line in the menu: what Norbelys does for them. */
  readonly summary: string;
  readonly glyph: GlyphName;
}

const team = (slug: string, fields: Omit<Team, "href" | "slug">): Team => ({
  ...fields,
  href: `/for/${slug}`,
  slug,
});

export const TEAMS: readonly Team[] = [
  team("sdrs", {
    glyph: "bell",
    label: "For SDRs",
    name: "SDRs",
    summary: "Hand off the sending and the follow-ups. Keep the conversations.",
  }),
  team("founders", {
    glyph: "frame",
    label: "For founders",
    name: "founders",
    summary: "Founder-led outbound from your own inbox, for $20 a month.",
  }),
  team("agencies", {
    glyph: "report",
    label: "For agencies",
    name: "agencies",
    summary: "One workspace per client, each with its own inboxes and keys.",
  }),
  team("recruiters", {
    glyph: "badge",
    label: "For recruiters",
    name: "recruiters",
    summary: "Write to every candidate by name, and stop when they answer.",
  }),
  team("account-executives", {
    glyph: "pawn",
    label: "For account executives",
    name: "account executives",
    summary: "Reach everyone on the deal. One reply pauses the rest.",
  }),
  team("growth-teams", {
    glyph: "fader",
    label: "For growth teams",
    name: "growth teams",
    summary:
      "Test every step, let replies pick the winner, pipe it all to your stack.",
  }),
];

/** Every team but one, for the "made for other teams too" row at the foot of a team's page. */
export const otherTeams = (slug: string): readonly Team[] =>
  TEAMS.filter((other) => other.slug !== slug);
