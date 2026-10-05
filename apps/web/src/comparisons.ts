/**
 * The comparison pages, as data: one entry per competitor, rendered by one template
 * (`pages/compare/[competitor].astro`). Every fact about a competitor carries the date it was
 * checked and a public source, and every entry says plainly where they are the better pick.
 */

/** A cell in the comparison table: yes, no, or a short phrase when the honest answer needs one. */
export type Answer = boolean | string;

export interface Comparison {
  /** The address: `/compare/<slug>`. */
  readonly slug: string;
  /** The competitor's name, written the way they write it. */
  readonly name: string;
  /** When the facts about them were last checked, as an ISO date. */
  readonly checked: string;
  /** The page's search description. */
  readonly description: string;
  /** One sentence on what each tool is built for. */
  readonly focus: { readonly them: string; readonly us: string };
  /** What only they do, what both do and what only we do, for the overlap diagram. */
  readonly overlap: {
    readonly them: readonly string[];
    readonly both: readonly string[];
    readonly us: readonly string[];
  };
  /** Who should pick which. */
  readonly choose: {
    readonly them: readonly string[];
    readonly us: readonly string[];
  };
  /** What it costs to start with each, in a sentence or two. */
  readonly price: { readonly them: string; readonly us: string };
  /** The feature table, in groups. */
  readonly table: readonly {
    readonly group: string;
    readonly rows: readonly {
      readonly label: string;
      readonly us: Answer;
      readonly them: Answer;
    }[];
  }[];
  /** Where each is genuinely stronger. */
  readonly strengths: {
    readonly them: readonly string[];
    readonly us: readonly string[];
  };
  /** How the two work together, when they do. */
  readonly together?: string;
  /** How to move over. */
  readonly switching: readonly string[];
  readonly faq: readonly (readonly [string, string])[];
  /** Where the facts about them come from: label and address. */
  readonly sources: readonly (readonly [string, string])[];
}

export const COMPETITORS: readonly Comparison[] = [
  {
    checked: "2026-10-05",
    choose: {
      them: [
        "You need to find the people to email, not just email them. Apollo’s database and enrichment are the heart of the product.",
        "Your team calls as much as it emails and wants a dialer in the same tool.",
        "You want Salesforce, HubSpot or Pipedrive kept in sync, or SCIM provisioning on an Organization plan.",
      ],
      us: [
        "You already have your lists, from a CSV, your CRM or an export from a data provider, and want to send from your own inboxes.",
        "You want one flat price instead of seats plus credits that expire every billing cycle.",
        "You want A/B tests, tracking and as many mailboxes as you need without moving up a tier.",
        "You build on your tools: signed webhooks, an API with idempotency keys, a CLI and an MCP server.",
      ],
    },
    description:
      "Norbelys vs Apollo, honestly: Apollo is a sales platform built on a contact database and a dialer; Norbelys is cold email from your own inboxes from $20 a month, with no credits. Prices, features and who should pick which.",
    faq: [
      [
        "Can I use Apollo and Norbelys together?",
        "Yes, and plenty of teams do. Apollo is good at finding people; Norbelys is built for emailing them from your own inboxes. Export a CSV from Apollo and import it into Norbelys.",
      ],
      [
        "Does Norbelys have a contact database like Apollo?",
        "No. Norbelys sends to the lists you bring: a CSV, your CRM through the API, or an export from a data provider like Apollo.",
      ],
      [
        "Is Norbelys cheaper than Apollo?",
        "For sending, usually. Norbelys starts at $20 a month with everything included. Apollo’s paid plans start at $49 per seat a month billed yearly, and tracking, A/B tests and unlimited mailboxes start on Professional at $79 per seat a month billed yearly. If you need Apollo’s data or its dialer, it isn’t a like-for-like comparison.",
      ],
      [
        "Does Apollo send from my own inboxes too?",
        "Yes. Apollo sends through the Gmail, Outlook or SMTP mailboxes you link, the same way Norbelys does.",
      ],
      [
        "Can I bring my sequences over?",
        "You rebuild them in Norbelys. Steps, delays and A/B variants carry over one to one, and every column of your CSV becomes a field you can write with.",
      ],
    ],
    focus: {
      them: "Apollo is a sales platform built around a contact database: you find people, enrich them, then email, call and track deals in one place.",
      us: "Norbelys does one job, cold email from your own inboxes, at one flat price from $20 a month with no credits to buy.",
    },
    name: "Apollo",
    overlap: {
      both: ["Sequences", "Unified inbox", "A/B tests", "API & MCP"],
      them: [
        "Contact database",
        "Phone dialer",
        "CRM sync",
        "Meetings & deals",
      ],
      us: ["One flat price", "No credits", "Webhooks for 19 events"],
    },
    price: {
      them: "Apollo has a free plan with 75 credits a month; paid plans start at $49 per seat a month billed yearly, and data, warm-up and extra mailboxes draw on credits that expire each cycle.",
      us: "Norbelys starts at $20 a month with every feature included.",
    },
    slug: "apollo",
    sources: [
      ["Apollo homepage", "https://www.apollo.io/"],
      ["Apollo pricing", "https://www.apollo.io/pricing"],
      [
        "Sending from your mailboxes",
        "https://knowledge.apollo.io/hc/en-us/articles/4409233349005",
      ],
      [
        "Mailbox rotation",
        "https://knowledge.apollo.io/hc/en-us/articles/4409396985741",
      ],
      [
        "Warm-up",
        "https://knowledge.apollo.io/hc/en-us/articles/26772718460045",
      ],
      [
        "A/B testing",
        "https://knowledge.apollo.io/hc/en-us/articles/4410749683597",
      ],
      [
        "Sequence rulesets",
        "https://knowledge.apollo.io/hc/en-us/articles/4409396858509",
      ],
      [
        "LinkedIn tasks",
        "https://knowledge.apollo.io/hc/en-us/articles/5646233248269",
      ],
      [
        "Apollo MCP",
        "https://knowledge.apollo.io/hc/en-us/articles/45119679436557",
      ],
      ["API rate limits", "https://docs.apollo.io/reference/rate-limits"],
    ],
    strengths: {
      them: [
        "Finding people. A large contact database, enrichment and intent data, none of which Norbelys has.",
        "Calling. A dialer on every paid plan, with voicemail drop and transcripts.",
        "Running a whole sales team in one tool: meetings, deals, CRM sync and a Chrome extension with about a million users.",
        "Security paperwork. Apollo says it holds SOC 2 Type II and ISO 27001; Norbelys doesn’t hold either yet.",
      ],
      us: [
        "A price you can predict: one flat price with everything in it, instead of seats plus credits that expire.",
        "Sending: A/B tests that pick their own winner, tracking, rotation and sending windows without moving up a tier.",
        "Building on it: 19 signed webhook events, an API with idempotency keys, a CLI and an MCP server.",
        "Focus. It does cold email and nothing else, so there’s much less to learn.",
      ],
    },
    switching: [
      "Export your contacts from Apollo as a CSV.",
      "Connect your Google, Microsoft or SMTP inboxes to Norbelys.",
      "Import the CSV. Every column becomes a field for your templates.",
      "Rebuild the sequence, set your sending window and daily limits, then start.",
    ],
    table: [
      {
        group: "Price",
        rows: [
          {
            label: "To start",
            them: "Free plan, then $49 per seat a month (Basic, billed yearly)",
            us: "$20 a month",
          },
          {
            label: "Credits",
            them: "Monthly credits for data, warm-up and extra mailboxes; they expire each cycle",
            us: "None",
          },
          {
            label: "Tracking, A/B tests and unlimited mailboxes",
            them: "Professional and up, $79 per seat a month billed yearly",
            us: "Included",
          },
        ],
      },
      {
        group: "Sending",
        rows: [
          {
            label: "Sends from your own Gmail, Outlook or SMTP inboxes",
            them: true,
            us: true,
          },
          { label: "Multi-step sequences", them: true, us: "Up to 50 steps" },
          {
            label: "A/B tests",
            them: "Email steps, Professional and up",
            us: "Every step, with an automatic winner",
          },
          { label: "Mailbox rotation", them: true, us: true },
          { label: "Daily limits and sending windows", them: true, us: true },
          {
            label: "Help for new inboxes",
            them: "Warm-up through Warmbox, paid plans",
            us: "Optional ramp over 8 days",
          },
          {
            label: "Stop when a colleague replies",
            them: "Optional rule",
            us: "Optional, per campaign",
          },
        ],
      },
      {
        group: "Replies and tracking",
        rows: [
          { label: "Unified inbox", them: true, us: true },
          {
            label: "Reply sorting",
            them: "Grouped by sentiment",
            us: "Person, auto-reply or out of office, with sentiment",
          },
          {
            label: "Open and click tracking",
            them: "Professional and up",
            us: "Off by default, bots left out",
          },
        ],
      },
      {
        group: "Data and calling",
        rows: [
          {
            label: "B2B contact database",
            them: "240M+ contacts, by Apollo’s count",
            us: false,
          },
          { label: "Enrichment", them: true, us: false },
          { label: "Phone dialer", them: "Every paid plan", us: false },
          { label: "LinkedIn", them: "Manual tasks", us: false },
          {
            label: "CRM sync",
            them: "Salesforce, HubSpot, Pipedrive",
            us: "Through the API and webhooks",
          },
        ],
      },
      {
        group: "Building on it",
        rows: [
          {
            label: "REST API",
            them: "Depends on plan, with daily limits",
            us: "Included",
          },
          {
            label: "Webhooks",
            them: "A workflow action",
            us: "19 events, signed",
          },
          { label: "MCP server", them: true, us: true },
          { label: "CLI", them: true, us: true },
        ],
      },
      {
        group: "Security",
        rows: [
          {
            label: "Single sign-on",
            them: "Organization plan, with SCIM",
            us: "OIDC, included",
          },
          {
            label: "SOC 2 Type II and ISO 27001",
            them: "Yes, by Apollo’s account",
            us: "Not yet",
          },
        ],
      },
    ],
    together:
      "Plenty of teams use both: find and export people in Apollo, then email them from your own inboxes in Norbelys. Export a CSV from Apollo and import it; every column becomes a field you can write with.",
  },
];
