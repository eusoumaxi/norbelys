import {
  AmazonIcon,
  CloudServerIcon,
  GoogleIcon,
  Mail02Icon,
  MailSend01Icon,
  MicrosoftIcon,
  ServerStack01Icon,
} from "@hugeicons/core-free-icons";
import type { IconSvgElement } from "@hugeicons/react";
import type {
  ImapSecurity,
  Provider,
  SmtpSecurity as Security,
} from "@norbelys/sdk";

import { humanize } from "@/lib/format";

/**
 * How an account comes in: a consent at Google or Microsoft (the provider names the address), an
 * SMTP login (its login is its address), a relay account the customer owns, or a login the
 * managed MTA provisions.
 */
type WayIn = "oauth" | "login" | "relay" | "managed";

/**
 * Whether a connection sends one cold message every few minutes like a person (`required`: the
 * mailboxes), may do so (`optional`: SES, as a paced sender of one address), or sends as fast as
 * its limits allow and refuses an interval (`refused`: SendGrid and Mailgun).
 * Managed mail supports optional exact minute pacing.
 */
type Pacing = "required" | "optional" | "refused";

/** What a relay's webhook verification material is called at its provider, and where to find it. */
interface WebhookKeyInfo {
  label: string;
  placeholder: string;
  description: string;
}

/** What a relay's API credential is: an AWS access key for SES, an API key alone otherwise. */
interface ApiCredentialInfo {
  /** The label of the key's id, when the credential has one (SES's access key id). */
  idLabel?: string;
  secretLabel: string;
  description: string;
}

/** The SMTP endpoint a provider's form starts from. */
interface SmtpDefaults {
  host: string;
  hostPlaceholder: string;
  hostDescription: string;
  port: number;
  security: Security;
  username: string;
  usernamePlaceholder: string;
  usernameDescription: string;
  passwordLabel: string;
  passwordDescription: string;
}

/** One way to connect an account: what its choice says and what its form asks for. */
export interface ProviderInfo {
  id: Provider;
  /** The choice's title on the connect page. */
  name: string;
  /** The short name tables use. */
  label: string;
  /** What kind of account it is, in a few words: `Google mailbox`, `Amazon SES relay`. */
  kind: string;
  icon: IconSvgElement;
  way: WayIn;
  /** The choice's sentence: how it connects and what Norbelys does with it. */
  summary: string;
  /** The daily limit the API gives a new connection of this provider. */
  dailyLimit: number;
  pacing: Pacing;
  smtp?: SmtpDefaults;
  webhookKey?: WebhookKeyInfo;
  apiCredential?: ApiCredentialInfo;
  /** What a quota scope's key names for this provider. */
  scopeKey: string;
}

const RELAY_PASSWORD =
  "The SMTP password; it is stored sealed and never shown again.";

/** Every provider the API connects, by its id. */
export const PROVIDERS: Record<Provider, ProviderInfo> = {
  google: {
    dailyLimit: 50,
    icon: GoogleIcon,
    id: "google",
    kind: "Google mailbox",
    label: "Google",
    name: "Google Workspace or Gmail",
    pacing: "required",
    scopeKey:
      "The ID of a Google Cloud project of your own whose limits several mailboxes share.",
    summary:
      "Sign in with Google. Norbelys sends as the mailbox and reads its replies.",
    way: "oauth",
  },
  mailgun: {
    apiCredential: {
      description:
        "Optional. Lets the daily check read the domain's events in the same region; a refused key is noted and sending goes on.",
      secretLabel: "Private API key",
    },
    dailyLimit: 10_000,
    icon: Mail02Icon,
    id: "mailgun",
    kind: "Mailgun relay",
    label: "Mailgun",
    name: "Mailgun",
    pacing: "refused",
    scopeKey:
      "The Mailgun account or domain whose limits its connections share.",
    smtp: {
      host: "smtp.mailgun.org",
      hostDescription:
        "smtp.mailgun.org, or smtp.eu.mailgun.org for an EU account.",
      hostPlaceholder: "smtp.mailgun.org",
      passwordDescription: RELAY_PASSWORD,
      passwordLabel: "SMTP password",
      port: 587,
      security: "starttls",
      username: "",
      usernameDescription: "The domain's SMTP login.",
      usernamePlaceholder: "postmaster@mg.example.com",
    },
    summary:
      "Your Mailgun domain, over SMTP. Its webhooks bring the delivery reports back.",
    way: "relay",
    webhookKey: {
      description:
        "Mailgun's HTTP webhook signing key, from its webhook settings. Without it, callbacks are refused.",
      label: "HTTP webhook signing key",
      placeholder: "key-…",
    },
  },
  microsoft: {
    dailyLimit: 50,
    icon: MicrosoftIcon,
    id: "microsoft",
    kind: "Microsoft mailbox",
    label: "Microsoft",
    name: "Microsoft 365 or Outlook",
    pacing: "required",
    scopeKey:
      "The Microsoft 365 tenant ID whose limits several mailboxes share.",
    summary:
      "Sign in with Microsoft. Norbelys sends as the mailbox and reads its replies.",
    way: "oauth",
  },
  norbelys: {
    dailyLimit: 500,
    icon: CloudServerIcon,
    id: "norbelys",
    kind: "Norbelys mail",
    label: "Norbelys mail",
    name: "Norbelys mail",
    pacing: "optional",
    scopeKey: "The domain service whose senders share its sending limits.",
    summary:
      "Connect your domain and send from its addresses with your workspace API key.",
    way: "managed",
  },
  sendgrid: {
    apiCredential: {
      description:
        "Optional. Lets Norbelys reconcile events through Email Activity; the SMTP key still sends.",
      secretLabel: "API key",
    },
    dailyLimit: 10_000,
    icon: MailSend01Icon,
    id: "sendgrid",
    kind: "SendGrid relay",
    label: "SendGrid",
    name: "SendGrid",
    pacing: "refused",
    scopeKey: "The SendGrid account whose limits its connections share.",
    smtp: {
      host: "smtp.sendgrid.net",
      hostDescription: "SendGrid's SMTP endpoint.",
      hostPlaceholder: "smtp.sendgrid.net",
      passwordDescription:
        "An API key with the Mail Send permission; each check reads its permissions. Stored sealed.",
      passwordLabel: "API key",
      port: 587,
      security: "starttls",
      username: "apikey",
      usernameDescription: "SendGrid's SMTP login is the word apikey.",
      usernamePlaceholder: "apikey",
    },
    summary:
      "Your SendGrid account, over SMTP. Its Event Webhook brings the delivery reports back.",
    way: "relay",
    webhookKey: {
      description:
        "The public key SendGrid's Event Webhook shows once signature verification is on (base64). Without it, callbacks are refused.",
      label: "Verification key",
      placeholder: "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE…",
    },
  },
  ses: {
    apiCredential: {
      description:
        "Optional. Lets the daily check read the account's sending state in the Region of the SMTP host and confirm each From address in SES. The SMTP login still sends.",
      idLabel: "AWS access key ID",
      secretLabel: "AWS secret access key",
    },
    dailyLimit: 10_000,
    icon: AmazonIcon,
    id: "ses",
    kind: "Amazon SES relay",
    label: "Amazon SES",
    name: "Amazon SES",
    pacing: "optional",
    scopeKey: "The AWS account and Region, such as 123456789012:eu-west-1.",
    smtp: {
      host: "",
      hostDescription: "The SMTP endpoint of the account's Region.",
      hostPlaceholder: "email-smtp.eu-west-1.amazonaws.com",
      passwordDescription: RELAY_PASSWORD,
      passwordLabel: "SMTP password",
      port: 587,
      security: "starttls",
      username: "",
      usernameDescription:
        "The SMTP user name SES gave with its SMTP password (SMTP settings, Create SMTP credentials).",
      usernamePlaceholder: "AKIA…",
    },
    summary:
      "Your Amazon SES account, over SMTP. Delivery reports come back through an SNS topic.",
    way: "relay",
    webhookKey: {
      description:
        "The SNS topic your configuration set publishes to. Subscribe the webhook URL this connection shows to it over HTTPS.",
      label: "SNS topic ARN",
      placeholder: "arn:aws:sns:eu-west-1:123456789012:norbelys-events",
    },
  },
  smtp: {
    dailyLimit: 50,
    icon: ServerStack01Icon,
    id: "smtp",
    kind: "SMTP mailbox",
    label: "SMTP",
    name: "Other mailbox (SMTP)",
    pacing: "required",
    scopeKey: "The mail server or account whose limits several logins share.",
    smtp: {
      host: "",
      hostDescription: "The server that accepts the mailbox's outgoing mail.",
      hostPlaceholder: "smtp.example.com",
      passwordDescription:
        "The mailbox's password or an app password. It is stored sealed and never shown again.",
      passwordLabel: "Password",
      port: 587,
      security: "starttls",
      username: "",
      usernameDescription: "",
      usernamePlaceholder: "",
    },
    summary:
      "Sign in with its SMTP login (an app password works). Replies are read over IMAP.",
    way: "login",
  },
};

/** The groups of the connect page, in order: what most people connect first, then the rest. */
export const PROVIDER_GROUPS: {
  id: string;
  title: string;
  description: string;
  providers: Provider[];
}[] = [
  {
    description:
      "A person's own mailbox. Norbelys sends from it at a person's pace and reads its replies.",
    id: "mailboxes",
    providers: ["google", "microsoft", "smtp"],
    title: "Your mailboxes",
  },
  {
    description:
      "Connect a provider account once, then manage its authorized senders. Norbelys mail uses your own domains.",
    id: "relays",
    providers: ["ses", "sendgrid", "mailgun", "norbelys"],
    title: "Sending services",
  },
];

/** The providers in the order of the connect page. */
export const PROVIDER_IDS: Provider[] = PROVIDER_GROUPS.flatMap(
  (group) => group.providers
);

/** Whether `value` names a provider the API connects (the API's list is open: new ones may come). */
export const isProvider = (value: string | null): value is Provider =>
  value !== null && Object.hasOwn(PROVIDERS, value);

/** A provider's catalogue entry, or none for a provider this dashboard does not know yet. */
export const providerInfo = (provider: string): ProviderInfo | undefined =>
  isProvider(provider) ? PROVIDERS[provider] : undefined;

/** `ses` → `Amazon SES`; an unknown provider as its own words. */
export const providerLabel = (provider: string): string =>
  providerInfo(provider)?.label ?? humanize(provider);

/** `google` → `Google mailbox`, `ses` → `Amazon SES relay`; an unknown provider as its own words. */
export const kindLabel = (provider: string): string =>
  providerInfo(provider)?.kind ?? humanize(provider);

/** The SMTP security modes, with their usual ports. */
export const SECURITY_OPTIONS: { label: string; value: Security }[] = [
  { label: "STARTTLS (port 587)", value: "starttls" },
  { label: "TLS (port 465)", value: "tls" },
  { label: "None (development only)", value: "plain" },
];

/** The IMAP security modes, with their usual ports. */
export const IMAP_SECURITY_OPTIONS: { label: string; value: ImapSecurity }[] = [
  { label: "TLS (port 993)", value: "tls" },
  { label: "None (development only)", value: "plain" },
];

const SMTP_PORTS: Record<Security, string> = {
  plain: "25",
  starttls: "587",
  tls: "465",
};

/**
 * The port after the security mode changes: the new mode's usual port when the old port was the
 * old mode's usual one, so a port the person typed is kept.
 */
export const portFor = (
  previous: Security,
  next: Security,
  port: string
): string => (port === SMTP_PORTS[previous] ? SMTP_PORTS[next] : port);

/**
 * The share of its daily limit, in percent, a warming connection may use at each stage: a tenth
 * at stage 0, the whole limit at stage 7, as the server ramps a new account over eight clean days.
 */
const WARMUP_SHARES = [10, 20, 30, 45, 60, 75, 90, 100];

/** The share of its daily limit, in percent, a connection at `stage` may use. */
export const warmupShare = (stage: number): number =>
  WARMUP_SHARES[stage] ?? 100;

/**
 * `30% of the daily limit (stage 2)`: a warm-up stage as a choice reads it, the share first and
 * the API's stage number after it for whoever sets it through the API.
 */
const warmupLabel = (stage: number): string =>
  `${warmupShare(stage)}% of the daily limit (stage ${stage})`;

/** The warm-up choice of a connection that is not warming. */
export const NOT_WARMING = "none";

/** The choices of a warm-up stage: not warming, or one of the ramp's stages. */
export const WARMUP_OPTIONS = [
  { label: "Off: the whole daily limit", value: NOT_WARMING },
  ...WARMUP_SHARES.map((_, stage) => ({
    label: warmupLabel(stage),
    value: String(stage),
  })),
];

/** A warm-up choice as the API takes it: a stage, or `null` for not warming. */
export const warmupStage = (choice: string | undefined): number | null =>
  choice && choice !== NOT_WARMING ? Number(choice) : null;

/** A connection's warm-up stage as a choice. */
export const warmupChoice = (stage: number | null | undefined): string =>
  stage === null || stage === undefined ? NOT_WARMING : String(stage);

const IMAP_PORTS: Record<ImapSecurity, string> = { plain: "143", tls: "993" };

/** The IMAP port after the security mode changes, as `portFor` does for SMTP. */
export const imapPortFor = (
  previous: ImapSecurity,
  next: ImapSecurity,
  port: string
): string => (port === IMAP_PORTS[previous] ? IMAP_PORTS[next] : port);

/**
 * What a warming connection may send today: its stage's share of the daily limit, rounded up and
 * at least one; the whole limit when it is not warming.
 */
export const allowance = (
  dailyLimit: number,
  stage: number | null | undefined
): number => {
  if (stage === null || stage === undefined) {
    return dailyLimit;
  }
  const share = WARMUP_SHARES[stage] ?? 100;
  return Math.max(1, Math.ceil((Math.max(dailyLimit, 1) * share) / 100));
};
