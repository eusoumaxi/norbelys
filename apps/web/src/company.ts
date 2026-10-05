/**
 * Who makes Norbelys and how to reach each part of the company: the one place the About,
 * Contact and legal pages read these details from.

 */
const REGIONS = "Germany and Finland";

export const COMPANY = {
  /** The company's full legal name: the party to the Terms, the DPA and the Privacy Policy. */
  name: "Xuxil, Inc.",
  /** Where it is incorporated, and what kind of company it is. */
  incorporation: "a Delaware corporation",
  state: "Delaware",
  country: "United States",
  /** Where formal notices go. */
  address: ["PO Box 7731", "Wilmington, DE 19899", "United States"],
  /** The year Norbelys launched. */
  launched: 2026,
  /** Where the hosted service runs. */
  regions: REGIONS,
  hosting: `Hetzner, in ${REGIONS}`,
  edge: "Cloudflare",
  /** Who takes card payments, as the documents name it in passing and in full. */
  payments: "Stripe",
  paymentsCompany: "Stripe, Inc.",
} as const;

/** One inbox at the company: who answers it, what it's for and what to put in the email. */
export interface Inbox {
  readonly id: string;
  readonly team: string;
  readonly address: string;
  /** What to write about, in a few words. */
  readonly for: string;
  /** How soon a person answers. */
  readonly reply: string;
  /** The same, short enough for the envelope's postmark. */
  readonly stamp: string;
  /** The subject the email starts with. */
  readonly subject: string;
  /** What helps the team answer in one go. */
  readonly include: readonly string[];
}

/** The company's inboxes, in the order a visitor is most likely to need them. */
export const INBOXES: readonly Inbox[] = [
  {
    address: "sales@norbelys.com",
    for: "Pricing, a walkthrough, moving your team over",
    id: "sales",
    include: [
      "How many people will send, and from how many inboxes",
      "What you use for outbound today",
      "Anything you must have before you switch",
    ],
    reply: "Within a working day",
    stamp: "1 day",
    subject: "Norbelys for our team",
    team: "Sales",
  },
  {
    address: "support@norbelys.com",
    for: "Help with your account, a campaign or a mailbox",
    id: "support",
    include: [
      "Your workspace’s name",
      "What you expected, and what happened instead",
      "The request ID from the error, if there was one",
    ],
    reply: "Within a working day",
    stamp: "1 day",
    subject: "Help with my workspace",
    team: "Support",
  },
  {
    address: "security@norbelys.com",
    for: "A vulnerability, or anything that looks unsafe",
    id: "security",
    include: [
      "What you found and how to reproduce it",
      "What an attacker could do with it",
      "How you’d like to be credited, if at all",
    ],
    reply: "Within 24 hours",
    stamp: "24 hours",
    subject: "Security report",
    team: "Security",
  },
  {
    address: "privacy@norbelys.com",
    for: "Your personal data, or a request about it",
    id: "privacy",
    include: [
      "The email address the request is about",
      "Whether you use Norbelys, or were emailed by someone who does",
      "What you’d like us to do: see, correct, delete or stop",
    ],
    reply: "Within 7 days",
    stamp: "7 days",
    subject: "Privacy request",
    team: "Privacy",
  },
  {
    address: "legal@norbelys.com",
    for: "Contracts, the DPA and formal notices",
    id: "legal",
    include: [
      "Your company’s legal name",
      "The document and the section it’s about",
      "Any deadline we should know about",
    ],
    reply: "Within 3 working days",
    stamp: "3 days",
    subject: "Legal question",
    team: "Legal",
  },
  {
    address: "abuse@norbelys.com",
    for: "Spam or abuse sent with Norbelys",
    id: "abuse",
    include: [
      "The whole email, with its headers",
      "When you received it",
      "The address it was sent to",
    ],
    reply: "Within 24 hours",
    stamp: "24 hours",
    subject: "Abuse report",
    team: "Abuse",
  },
  {
    address: "press@norbelys.com",
    for: "Interviews, stories and brand files",
    id: "press",
    include: ["Who you write for", "What the story is about", "Your deadline"],
    reply: "Within 2 working days",
    stamp: "2 days",
    subject: "Press enquiry",
    team: "Press",
  },
  {
    address: "hello@norbelys.com",
    for: "Anything else, or just to say hello",
    id: "hello",
    include: ["Whatever you’d like us to know"],
    reply: "Within 2 working days",
    stamp: "2 days",
    subject: "Hello",
    team: "Everyone else",
  },
];

/** The general inbox, for anything that fits none of the others, and replies to the About page. */
export const HELLO = "hello@norbelys.com";

/** A `mailto:` address with a subject (and, when given, a body) already filled in. */
export const mailto = (
  address: string,
  subject: string,
  body?: string
): string => {
  const query = new URLSearchParams({ subject });
  if (body !== undefined) {
    query.set("body", body);
  }
  // Mail clients read `+` literally; spaces must travel as %20.
  return `mailto:${address}?${query.toString().replaceAll("+", "%20")}`;
};
