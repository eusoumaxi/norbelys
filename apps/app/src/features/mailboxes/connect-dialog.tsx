import { HugeiconsIcon } from "@hugeicons/react";
import type {
  ConnectionObject,
  CreateConnection,
  ImapSecurity,
  SmtpSecurity as Security,
} from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import type { SubmitEvent } from "react";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { SaveFailure } from "@/components/problem";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { domainOptionsQuery } from "@/features/domains/queries";
import {
  Endpoint,
  FormSection,
  inputId,
  integer,
  optional,
  SelectRow,
  splitList,
  TextRow,
  useValues,
} from "@/features/mailboxes/form";
import type { SetValue, Values } from "@/features/mailboxes/form";
import {
  NOT_WARMING,
  providerInfo,
  WARMUP_OPTIONS,
  warmupStage,
} from "@/features/mailboxes/providers";
import type { ProviderInfo } from "@/features/mailboxes/providers";
import { connectionsKey } from "@/features/mailboxes/queries";
import { Reveal } from "@/features/mailboxes/reveal";
import { ScopeSelect } from "@/features/mailboxes/scope-select";
import { FormField } from "@/lib/form";
import { formatCount } from "@/lib/format";
import {
  describeProblem,
  fieldProblems,
  problemAt,
  unplacedProblems,
} from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

type Problems = Readonly<Record<string, string>>;

/** Every field path a connect form shows, so a problem of another path is shown under the form. */
const FIELDS = [
  "account_email",
  "smtp",
  "imap",
  "identities",
  "webhook",
  "api_credential",
  "quota_scope_id",
  "daily_limit",
  "send_interval_minutes",
  "warmup_stage",
  "receiving",
];

/** Whether IMAP is read: a choice of the form, not a field of the API. */
const READ_IMAP = "read_imap";

const initialValues = (info: ProviderInfo): Values => ({
  account_email: "",
  "api_credential.id": "",
  "api_credential.secret": "",
  daily_limit: String(info.dailyLimit),
  identities: "",
  "imap.host": "",
  "imap.port": "993",
  "imap.security": "tls",
  quota_scope_id: "",
  [READ_IMAP]: "yes",
  send_interval_minutes: info.pacing === "required" ? "10" : "",
  "smtp.configuration_set": "",
  "smtp.host": info.smtp?.host ?? "",
  "smtp.password": "",
  "smtp.port": String(info.smtp?.port ?? 587),
  "smtp.security": info.smtp?.security ?? "starttls",
  "smtp.username": info.smtp?.username ?? "",
  warmup_stage: NOT_WARMING,
  "webhook.key": "",
});

/** The fields a provider's form cannot be sent without; the API's own checks still apply. */
const requiredFields = (info: ProviderInfo, values: Values): string[] => {
  switch (info.way) {
    case "oauth": {
      return [];
    }
    case "managed": {
      return ["account_email"];
    }
    case "login": {
      return [
        "account_email",
        "smtp.host",
        "smtp.port",
        "smtp.password",
        ...(values[READ_IMAP] ? ["imap.host", "imap.port"] : []),
      ];
    }
    default: {
      const ses = info.id === "ses";
      const apiKey =
        ses &&
        Boolean(
          values["api_credential.id"]?.trim() ||
          values["api_credential.secret"]?.trim()
        );
      return [
        "account_email",
        "smtp.host",
        "smtp.port",
        "smtp.username",
        "smtp.password",
        ...(ses ? ["smtp.configuration_set", "quota_scope_id"] : []),
        ...(apiKey ? ["api_credential.id", "api_credential.secret"] : []),
      ];
    }
  }
};

const smtpOf = (info: ProviderInfo, values: Values) =>
  info.smtp
    ? {
        configuration_set:
          info.id === "ses"
            ? optional(values, "smtp.configuration_set")
            : undefined,
        host: values["smtp.host"]?.trim() ?? "",
        password: values["smtp.password"] ?? "",
        port: integer(values, "smtp.port") ?? 0,
        security: (values["smtp.security"] ?? "starttls") as Security,
        username:
          info.way === "relay" ? optional(values, "smtp.username") : undefined,
      }
    : undefined;

const imapOf = (info: ProviderInfo, values: Values) =>
  info.way === "login" && values[READ_IMAP]
    ? {
        host: values["imap.host"]?.trim() ?? "",
        port: integer(values, "imap.port") ?? 0,
        security: (values["imap.security"] ?? "tls") as ImapSecurity,
      }
    : undefined;

/** The body of `connections.create` for a provider's form. */
const createBody = (
  info: ProviderInfo,
  values: Values,
  returnTo: string
): CreateConnection => {
  const pacing = {
    daily_limit: integer(values, "daily_limit"),
    provider: info.id,
    quota_scope_id: optional(values, "quota_scope_id"),
    send_interval_minutes:
      info.pacing === "refused"
        ? undefined
        : integer(values, "send_interval_minutes"),
    warmup_stage: warmupStage(values.warmup_stage) ?? undefined,
  };
  if (info.way === "oauth") {
    return { ...pacing, return_to: returnTo };
  }
  const identities = splitList(values.identities).map((email) => ({ email }));
  const key = optional(values, "webhook.key");
  return {
    ...pacing,
    account_email: optional(values, "account_email"),
    identities:
      info.way === "relay" && identities.length > 0 ? identities : undefined,
    imap: imapOf(info, values),
    smtp: smtpOf(info, values),
    webhook: info.webhookKey && key ? { key } : undefined,
  };
};

/** What connecting led to: a consent page to open, or the connection (and a credential refused). */
type Connected =
  | { consent: string }
  | { connection: ConnectionObject; credentialFailure?: unknown };

/**
 * Connects the account. A relay's API credential is not part of a create, so it is saved by an
 * update right after; if that update fails the connection stays, and the failure is returned.
 */
const connectAccount = async (
  workspace: Workspace,
  info: ProviderInfo,
  values: Values
): Promise<Connected> => {
  const created = await workspace.api.connections.create(
    createBody(info, values, `/w/${workspace.slug}/mailboxes/new`)
  );
  // A Google or Microsoft mailbox answers the consent to open (`202`) instead of the connection.
  if (!("id" in created)) {
    return { consent: created.authorization.url };
  }
  const secret = values["api_credential.secret"]?.trim();
  if (!info.apiCredential || !secret) {
    return { connection: created };
  }
  try {
    const connection = await workspace.api.connections.update(created.id, {
      api_credential: { id: optional(values, "api_credential.id"), secret },
    });
    return { connection };
  } catch (error) {
    return { connection: created, credentialFailure: error };
  }
};

/** The account's address or name, as each way in calls it. */
const accountText = (info: ProviderInfo) => {
  if (info.way === "login") {
    return {
      description:
        "The mailbox's address, which is also its login. Connecting it again later brings it back with its history.",
      label: "Address",
      placeholder: "ada@example.com",
    };
  }
  if (info.way === "managed") {
    return {
      description:
        "An address on a sending domain you verified; Norbelys creates its login on its own mail server.",
      label: "Address",
      placeholder: "ada@example.com",
    };
  }
  return {
    description:
      info.pacing === "optional"
        ? "A name for this account, such as ses-eu-west-1; or, when it sends at a person's pace, the one From address it paces."
        : "A name for this account, such as its account name, or the address it sends as.",
    label: "Account",
    placeholder: info.pacing === "optional" ? "ses-eu-west-1" : "marketing",
  };
};

interface SectionProps {
  info: ProviderInfo;
  problems: Problems;
  set: SetValue;
  values: Values;
}

/** The verified sending domains a hosted login's address may be on. */
const VerifiedDomains = () => {
  const workspace = useWorkspace();
  const domains = useQuery(domainOptionsQuery(workspace));
  if (!domains.data) {
    return null;
  }
  const verified = domains.data.data.filter((domain) => domain.verified_at);
  const link = (
    <Link
      className="text-link hover:text-link-hover"
      params={{ slug: workspace.slug }}
      to="/w/$slug/domains"
    >
      Sending domains
    </Link>
  );
  if (verified.length === 0) {
    return (
      <p className="text-warning text-xs">
        No sending domain is verified yet. Add one and publish its records
        first: {link}.
      </p>
    );
  }
  return (
    <p className="text-fg-3 text-xs">
      Verified:{" "}
      <span className="text-fg-2 font-mono">
        {verified.map((domain) => domain.hostname).join(", ")}
      </span>{" "}
      ({link}).
    </p>
  );
};

const AccountSection = ({ info, problems, set, values }: SectionProps) => {
  const text = accountText(info);
  return (
    <div className="flex flex-col gap-2">
      <TextRow
        autoComplete="off"
        description={text.description}
        label={text.label}
        name="account_email"
        placeholder={text.placeholder}
        problems={problems}
        required
        set={set}
        type={info.way === "relay" ? "text" : "email"}
        values={values}
      />
      {info.way === "managed" ? <VerifiedDomains /> : null}
    </div>
  );
};

const SmtpSection = ({ info, problems, set, values }: SectionProps) => {
  const { smtp } = info;
  if (!smtp) {
    return null;
  }
  return (
    <FormSection
      description={
        info.way === "relay"
          ? `${info.label}'s SMTP server. Norbelys signs in with this before anything is sent, to make sure it works.`
          : "The server the mailbox sends through. Norbelys signs in with this before anything is sent, to make sure it works."
      }
      title="Sign-in"
    >
      <Endpoint
        hostDescription={smtp.hostDescription}
        hostPlaceholder={smtp.hostPlaceholder}
        prefix="smtp"
        problems={problems}
        required
        set={set}
        values={values}
      />
      {info.way === "relay" ? (
        <TextRow
          autoComplete="off"
          description={smtp.usernameDescription}
          label="SMTP login"
          mono
          name="smtp.username"
          placeholder={smtp.usernamePlaceholder}
          problems={problems}
          required
          set={set}
          values={values}
        />
      ) : null}
      <TextRow
        autoComplete="new-password"
        description={smtp.passwordDescription}
        label={smtp.passwordLabel}
        name="smtp.password"
        problems={problems}
        required
        set={set}
        type="password"
        values={values}
      />
      {info.id === "ses" ? (
        <TextRow
          autoComplete="off"
          description="The SES configuration set every message names, so SES publishes its events to your SNS topic. Letters, digits, - and _."
          label="Configuration set"
          mono
          name="smtp.configuration_set"
          placeholder="norbelys-events"
          problems={problems}
          required
          set={set}
          values={values}
        />
      ) : null}
    </FormSection>
  );
};

const ImapSection = ({ problems, set, values }: SectionProps) => (
  <FormSection
    description="Norbelys reads replies over IMAP with the same login and password. Without it, replies to this mailbox are not seen here."
    title="Replies"
  >
    <Label className="text-fg gap-2 font-normal">
      <Checkbox
        checked={Boolean(values[READ_IMAP])}
        onCheckedChange={(checked) => set(READ_IMAP, checked ? "yes" : "")}
      />
      Read replies over IMAP
    </Label>
    <Reveal
      className="flex flex-col gap-4"
      gap={4}
      open={Boolean(values[READ_IMAP])}
    >
      <Endpoint
        hostPlaceholder="imap.example.com"
        prefix="imap"
        problems={problems}
        required
        set={set}
        values={values}
      />
    </Reveal>
  </FormSection>
);

const RelaySection = ({ info, problems, set, values }: SectionProps) => {
  const identitiesProblem = problemAt(problems, "identities");
  return (
    <FormSection
      description={`The addresses its emails come from. ${info.label} must allow each of them. Names and signatures are added on the mailbox's page.`}
      title="Senders"
    >
      <FormField
        description="One address per line. When the account above is an address and this stays empty, that address is the one sender."
        htmlFor={inputId("identities")}
        label="Addresses"
        optional
        problem={identitiesProblem}
      >
        <Textarea
          aria-invalid={Boolean(identitiesProblem)}
          className="font-mono"
          id={inputId("identities")}
          onChange={(event) => set("identities", event.target.value)}
          placeholder={"ada@example.com\nsales@example.com"}
          value={values.identities ?? ""}
        />
      </FormField>
    </FormSection>
  );
};

const WebhookSection = ({ info, problems, set, values }: SectionProps) => {
  const { webhookKey } = info;
  if (!webhookKey) {
    return null;
  }
  return (
    <FormSection
      description={`${info.label} reports what happened to each email through a webhook; its URL shows once connected. The reports are signed with this key and refused until it is saved. You can add it later.`}
      title="Delivery reports"
    >
      <TextRow
        autoComplete="off"
        description={webhookKey.description}
        label={webhookKey.label}
        mono
        name="webhook.key"
        optional
        placeholder={webhookKey.placeholder}
        problems={problems}
        set={set}
        values={values}
      />
    </FormSection>
  );
};

const ApiCredentialSection = ({
  info,
  problems,
  set,
  values,
}: SectionProps) => {
  const credential = info.apiCredential;
  if (!credential) {
    return null;
  }
  return (
    <FormSection
      description={`${credential.description} Saved right after the connection, sealed, and never shown again.`}
      title="API credential"
    >
      {credential.idLabel ? (
        <TextRow
          autoComplete="off"
          label={credential.idLabel}
          mono
          name="api_credential.id"
          optional
          placeholder="AKIA…"
          problems={problems}
          set={set}
          values={values}
        />
      ) : null}
      <TextRow
        autoComplete="new-password"
        label={credential.secretLabel}
        name="api_credential.secret"
        optional
        problems={problems}
        set={set}
        type="password"
        values={values}
      />
    </FormSection>
  );
};

/**
 * The provider account whose limits the connection shares with others (the API's quota scope):
 * required for SES, whose account and Region every SES connection names; optional otherwise.
 */
const ScopeSection = ({ info, problems, set, values }: SectionProps) => {
  const required = info.id === "ses";
  return (
    <FormField
      description={
        required
          ? "The SES account and Region whose limits its connections share. Its webhook URL is the one the account's SNS topic posts to. The API calls it a quota scope."
          : "When several mailboxes share one provider account's limits, name that account so Norbelys keeps them under its limits together. The API calls it a quota scope."
      }
      htmlFor={inputId("quota_scope_id")}
      label="Shared limit"
      optional={!required}
      problem={problemAt(problems, "quota_scope_id")}
    >
      <ScopeSelect
        id={inputId("quota_scope_id")}
        onChange={(value) => set("quota_scope_id", value)}
        provider={info.id}
        required={required}
        value={values.quota_scope_id ?? ""}
      />
    </FormField>
  );
};

const intervalText = (info: ProviderInfo) =>
  info.pacing === "optional"
    ? "Leave it empty to send as fast as its limits allow. With minutes, it sends one campaign email at a time, as one address: its account's."
    : "It waits this long between two campaign emails, like a person writing them: 5 to 1,440, in steps of 5.";

/** The fields whose problems open the pace section by themselves. */
const PACE_FIELDS = [
  "daily_limit",
  "send_interval_minutes",
  "warmup_stage",
  "quota_scope_id",
];

/** How much a new connection sends, in one sentence, from the values of its form. */
const paceSummary = (info: ProviderInfo, values: Values): string => {
  const daily = integer(values, "daily_limit");
  const minutes =
    info.pacing === "refused"
      ? undefined
      : integer(values, "send_interval_minutes");
  const warming = warmupStage(values.warmup_stage) !== null;
  return [
    daily === undefined
      ? "No daily limit set yet"
      : `Up to ${formatCount(daily)} emails a day`,
    minutes === undefined
      ? ", as fast as its limits allow. "
      : `, one campaign email every ${formatCount(minutes)} minutes. `,
    warming ? "Warming up from 10% of the limit." : "No warm-up.",
  ].join("");
};

/**
 * How much the new connection sends, as one sentence with a button that opens its fields: the
 * daily limit, the minutes between campaign emails, the warm-up and, when it is optional, the
 * shared limit. The defaults suit most mailboxes, so the fields stay closed until asked for, or
 * until the API finds a problem with one of them.
 */
const PacingSection = (props: SectionProps) => {
  const { info, problems, set, values } = props;
  const [open, setOpen] = useState(false);
  const flagged = PACE_FIELDS.some((field) => problemAt(problems, field));
  const shown = open || flagged;
  return (
    <section className="flex min-w-0 flex-col">
      <div className="flex items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h3 className="text-fg text-base font-medium">How much it sends</h3>
          <p className="text-fg-2 text-xs">
            {paceSummary(info, values)} You can change it any time.
          </p>
        </div>
        <Button
          aria-expanded={shown}
          disabled={flagged}
          onClick={() => setOpen(!open)}
          size="s"
          type="button"
          variant="secondary"
        >
          {shown ? "Done" : "Change"}
        </Button>
      </div>
      <Reveal className="flex flex-col gap-4 pt-4" open={shown}>
        <div className="grid gap-4 sm:grid-cols-2">
          <TextRow
            description="The most emails it sends in a day (UTC)."
            inputMode="numeric"
            label="Daily limit"
            max={1_000_000}
            min={1}
            name="daily_limit"
            problems={problems}
            set={set}
            type="number"
            values={values}
          />
          {info.pacing === "refused" ? null : (
            <TextRow
              description={intervalText(info)}
              inputMode="numeric"
              label="Minutes between campaign emails"
              max={1440}
              min={5}
              name="send_interval_minutes"
              optional={info.pacing === "optional"}
              problems={problems}
              set={set}
              step={5}
              type="number"
              values={values}
            />
          )}
        </div>
        <SelectRow
          description="A new mailbox earns its daily limit slowly: it starts at 10% of it, and each clean day raises it until it may use all of it."
          label="Warm-up"
          name="warmup_stage"
          options={WARMUP_OPTIONS}
          problems={problems}
          set={set}
          values={values}
        />
        {info.id === "ses" || info.way === "managed" ? null : (
          <ScopeSection {...props} />
        )}
      </Reveal>
    </section>
  );
};

/** The sections a provider's form shows, in order. */
const Sections = (props: SectionProps) => {
  const { info } = props;
  if (info.way === "oauth") {
    return (
      <>
        <p className="text-fg-2 text-sm">
          You go to {info.label} to choose the mailbox and allow Norbelys to use
          it, then come back here. The address comes from {info.label}, and its
          inbox is read for replies.
        </p>
        <PacingSection {...props} />
      </>
    );
  }
  return (
    <>
      <AccountSection {...props} />
      <SmtpSection {...props} />
      {info.way === "login" ? <ImapSection {...props} /> : null}
      {info.way === "relay" ? (
        <>
          <RelaySection {...props} />
          <WebhookSection {...props} />
          <ApiCredentialSection {...props} />
        </>
      ) : null}
      {info.id === "ses" ? <ScopeSection {...props} /> : null}
      <PacingSection {...props} />
    </>
  );
};

/** What the submit button says for a provider. */
const submitLabel = (info: ProviderInfo) =>
  info.way === "oauth" ? `Continue to ${info.label}` : "Connect";

const ConnectForm = ({ info }: { info: ProviderInfo }) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { set, values } = useValues(() => initialValues(info));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const ready = requiredFields(info, values).every((name) =>
    values[name]?.trim()
  );

  const submit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const outcome = await connectAccount(workspace, info, values);
      if ("consent" in outcome) {
        // The provider's consent page; the API brings the browser back to this page.
        window.location.assign(outcome.consent);
        return;
      }
      await queryClient.invalidateQueries({
        queryKey: connectionsKey(workspace),
      });
      const params = {
        connectionId: outcome.connection.id,
        slug: workspace.slug,
      };
      if (outcome.credentialFailure) {
        toast.error(
          `Connected, but the API credential was not saved: ${describeProblem(outcome.credentialFailure).detail}`
        );
        await navigate({
          params,
          to: "/w/$slug/mailboxes/$connectionId/settings",
        });
        return;
      }
      toast.success("Mailbox connected. Norbelys is checking it.");
      await navigate({ params, to: "/w/$slug/mailboxes/$connectionId" });
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <form className="flex min-h-0 flex-col" onSubmit={submit}>
      <DialogHeader>
        <div className="flex items-center gap-3">
          <span className="bg-chrome text-icon flex size-8 shrink-0 items-center justify-center rounded-sm">
            <HugeiconsIcon className="size-4" icon={info.icon} />
          </span>
          <DialogTitle>{info.name}</DialogTitle>
        </div>
        <DialogDescription>{info.summary}</DialogDescription>
      </DialogHeader>
      <DialogBody className="gap-6">
        <Sections info={info} problems={problems} set={set} values={values} />
        <SaveFailure
          failure={failure}
          unplaced={unplacedProblems(problems, FIELDS)}
        />
      </DialogBody>
      <DialogActions>
        <SubmitButton busy={busy} disabled={!ready}>
          {submitLabel(info)}
        </SubmitButton>
      </DialogActions>
    </form>
  );
};

/**
 * The form that connects one provider's account, in a dialog: the fields the API requires for
 * that provider (an OAuth consent, an SMTP login with IMAP, a relay's SMTP credential with its
 * webhook key and API credential, or an address for the hosted mail), each said on the form, with
 * the API's field problems shown on their inputs. `provider` is the open card; the form starts
 * empty each time a card opens.
 */
export const ConnectDialog = ({
  onOpenChange,
  onOpenChangeComplete,
  open,
  provider,
}: {
  onOpenChange: (open: boolean) => void;
  onOpenChangeComplete?: (open: boolean) => void;
  open: boolean;
  provider: string | null;
}) => {
  const info = provider ? providerInfo(provider) : undefined;
  return (
    <Dialog
      onOpenChange={onOpenChange}
      onOpenChangeComplete={onOpenChangeComplete}
      open={open && Boolean(info)}
    >
      <DialogContent className="max-w-[640px]">
        {info ? <ConnectForm info={info} key={info.id} /> : null}
      </DialogContent>
    </Dialog>
  );
};
