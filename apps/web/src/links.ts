import { TEAMS } from "./solutions";

/** Public destinations shared by the site's navigation and calls to action. */
export const APP = "https://app.norbelys.com/sign-in";
/** Where a new team starts: the same door, since signing in creates the account. */
export const START = APP;
/** The product documentation, independent of the marketing site. */
export const DOCS = "https://docs.norbelys.com";
/** The API reference, for the people who build on Norbelys. */
export const DEVELOPERS = `${DOCS}/reference`;
/** The blog, published on the marketing site. */
export const BLOG = "/blog";
/** The source and licence of the product. */
export const SOURCE = "https://github.com/eusoumaxi/norbelys";

/** The site's own pages. */
export const PAGES = {
  about: "/about",
  acceptableUse: "/legal/acceptable-use",
  aiPersonalization: "/ai-personalization",
  campaigns: "/campaigns",
  cli: "/developers#cli",
  contact: "/contact",
  developers: "/developers",
  dpa: "/legal/dpa",
  legal: "/legal",
  mcp: "/developers#mcp",
  privacy: "/legal/privacy",
  sequences: "/campaigns#drive",
  solutions: "/solutions",
  subprocessors: "/legal/subprocessors",
  terms: "/legal/terms",
  webhooks: "/developers#webhooks",
} as const;

/** The competitors a buyer compares us with, each with its own comparison page. */
export const COMPARISONS = [
  ["Smartlead", "/compare/smartlead"],
  ["Instantly", "/compare/instantly"],
  ["Lemlist", "/compare/lemlist"],
  ["Apollo", "/compare/apollo"],
  ["Saleshandy", "/compare/saleshandy"],
  ["Reply.io", "/compare/reply"],
  ["Woodpecker", "/compare/woodpecker"],
  ["Snov.io", "/compare/snov"],
] as const;

/** The teams Norbelys is built for, each with a page of its own (solutions.ts describes them). */
export const SOLUTIONS = TEAMS.map((team) => [team.label, team.href] as const);
