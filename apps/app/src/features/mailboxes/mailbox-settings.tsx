import type {
  ConnectionObject,
  ImapSecurity,
  SmtpPatch,
  UpdateConnection,
} from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { cn } from "cn";
import type { MouseEvent, ReactNode, SubmitEvent } from "react";
import { useState } from "react";
import { toast } from "sonner";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { CodeLine, Copyable } from "@/components/copy";
import { DetailList } from "@/components/details";
import { SaveFailure } from "@/components/problem";
import { SettingsPanel } from "@/components/settings-layout";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import {
  Endpoint,
  inputId,
  integer,
  optional,
  SelectRow,
  splitList,
  TextRow,
} from "@/features/mailboxes/form";
import type { SetValue, Values } from "@/features/mailboxes/form";
import { canChange } from "@/features/mailboxes/mailbox-header";
import { WebhookKeyBadge } from "@/features/mailboxes/parts";
import {
  providerInfo,
  providerLabel,
  WARMUP_OPTIONS,
  warmupChoice,
  warmupStage,
} from "@/features/mailboxes/providers";
import type { ProviderInfo } from "@/features/mailboxes/providers";
import {
  connectionQuery,
  connectionsKey,
  updateGuarded,
} from "@/features/mailboxes/queries";
import { Reveal } from "@/features/mailboxes/reveal";
import { ScopeSelect } from "@/features/mailboxes/scope-select";
import { useAction } from "@/lib/actions";
import { FormField, TimeZoneOptions } from "@/lib/form";
import { formatDateTime, WEEKDAYS } from "@/lib/format";
import { fieldProblems, problemAt, unplacedProblems } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

type Save = (values: Values, base: Values) => Promise<ConnectionObject>;

/** A save refused because the list it replaces changed meanwhile: the latest connection, and why. */
class ChangedError extends Error {
  override name = "ChangedError";
  readonly latest: ConnectionObject;

  constructor(latest: ConnectionObject, message: string) {
    super(message);
    this.latest = latest;
  }
}

/** Whether a value of the form differs from the one it started from. */
const moved = (values: Values, base: Values, name: string) =>
  (values[name] ?? "") !== (base[name] ?? "");

/**
 * The keyboard's place once a panel's buttons leave: they show only while something changed, so
 * after a save or a discard the focus goes back to the panel's form instead of the page.
 */
const refocus = (form: HTMLFormElement | null) => {
  if (
    form &&
    (form.contains(document.activeElement) ||
      document.activeElement === document.body)
  ) {
    form.focus({ preventScroll: true });
  }
};

/**
 * One settings panel's form: its values, which start from the connection and follow a newer read
 * of it while nothing is edited, and its save, which shows the saved connection at once.
 */
const usePanel = (
  connection: ConnectionObject,
  initialOf: (connection: ConnectionObject) => Values,
  save: Save,
  done: string
) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const start = initialOf(connection);
  const [base, setBase] = useState(start);
  const [values, setValues] = useState(start);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const dirty = JSON.stringify(values) !== JSON.stringify(base);
  if (!dirty && !busy && JSON.stringify(start) !== JSON.stringify(base)) {
    setBase(start);
    setValues(start);
  }
  const set: SetValue = (name, value) => {
    setValues((current) => ({ ...current, [name]: value }));
  };
  const display = (saved: ConnectionObject) => {
    queryClient.setQueryData(
      connectionQuery(workspace, saved.id).queryKey,
      saved
    );
    void queryClient.invalidateQueries({
      queryKey: connectionsKey(workspace),
    });
    const next = initialOf(saved);
    setBase(next);
    setValues(next);
  };
  const handleSubmit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = event.currentTarget;
    setBusy(true);
    setFailure(null);
    try {
      display(await save(values, base));
      toast.success(done);
      refocus(form);
    } catch (error) {
      if (error instanceof ChangedError) {
        display(error.latest);
        toast.warning(error.message);
      } else {
        setFailure(error);
      }
    }
    setBusy(false);
  };
  return {
    busy,
    dirty,
    failure,
    handleReset: (event: MouseEvent<HTMLButtonElement>) => {
      const { form } = event.currentTarget;
      setValues(base);
      setFailure(null);
      refocus(form);
    },
    handleSubmit,
    problems: fieldProblems(failure),
    set,
    values,
  };
};

type Panel = ReturnType<typeof usePanel>;

/**
 * A settings panel holding one form: its fields, what failed, and Save beside Discard, which open
 * under the fields once something changed and close once it is saved or discarded. `id` is the
 * panel's anchor, where a notice's button lands.
 */
const PanelForm = ({
  children,
  description,
  fields,
  id,
  locked,
  panel,
  submitLabel = "Save",
  title,
}: {
  children: ReactNode;
  description: ReactNode;
  fields: string[];
  id?: string;
  locked: boolean;
  panel: Panel;
  submitLabel?: string;
  title: string;
}) => (
  <div className="scroll-mt-6" id={id}>
    <SettingsPanel description={description} title={title}>
      <form
        className="flex flex-col outline-none"
        onSubmit={panel.handleSubmit}
        tabIndex={-1}
      >
        <fieldset className="flex min-w-0 flex-col gap-4" disabled={locked}>
          {children}
        </fieldset>
        <Reveal className="pt-4" open={Boolean(panel.failure)}>
          <SaveFailure
            failure={panel.failure}
            unplaced={unplacedProblems(panel.problems, fields)}
          />
        </Reveal>
        <Reveal className="pt-4" open={!locked && (panel.dirty || panel.busy)}>
          <div className="flex items-center gap-2">
            <Button disabled={panel.busy} type="submit" variant="primary">
              {panel.busy ? <Spinner /> : null}
              {submitLabel}
            </Button>
            <Button
              disabled={panel.busy}
              onClick={panel.handleReset}
              type="button"
              variant="tertiary"
            >
              Discard
            </Button>
          </div>
        </Reveal>
      </form>
    </SettingsPanel>
  </div>
);

interface PanelProps {
  connection: ConnectionObject;
  info: ProviderInfo | undefined;
  locked: boolean;
}

const show = (value: number | null | undefined) =>
  value === null || value === undefined ? "" : String(value);

/** How much it sends: the daily limit, the minutes between campaign emails, the warm-up. */
const PacePanel = ({ connection, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const paced =
    connection.send_interval_minutes !== null &&
    connection.send_interval_minutes !== undefined;
  const panel = usePanel(
    connection,
    (c) => ({
      daily_limit: String(c.daily_limit),
      send_interval_minutes: show(c.send_interval_minutes),
      warmup_stage: warmupChoice(c.warmup_stage),
    }),
    (values, base) => {
      const body: UpdateConnection = {};
      if (moved(values, base, "daily_limit")) {
        body.daily_limit = integer(values, "daily_limit");
      }
      if (paced && moved(values, base, "send_interval_minutes")) {
        body.send_interval_minutes = integer(values, "send_interval_minutes");
      }
      if (moved(values, base, "warmup_stage")) {
        body.warmup_stage = warmupStage(values.warmup_stage);
      }
      return workspace.api.connections.update(connection.id, body);
    },
    "Saved: how much it sends"
  );
  return (
    <PanelForm
      description="Every sender of this mailbox shares these, so adding a sender never adds volume."
      fields={["daily_limit", "send_interval_minutes", "warmup_stage"]}
      locked={locked}
      panel={panel}
      title="How much it sends"
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <TextRow
          description="The most emails it sends in a day (UTC), from 1 to 1,000,000."
          inputMode="numeric"
          label="Daily limit"
          max={1_000_000}
          min={1}
          name="daily_limit"
          problems={panel.problems}
          set={panel.set}
          type="number"
          values={panel.values}
        />
        {paced ? (
          <TextRow
            description="It waits this long between two campaign emails, like a person writing them: 5 to 1,440, in steps of 5."
            inputMode="numeric"
            label="Minutes between campaign emails"
            max={1440}
            min={5}
            name="send_interval_minutes"
            problems={panel.problems}
            set={panel.set}
            step={5}
            type="number"
            values={panel.values}
          />
        ) : (
          <FormField label="Minutes between campaign emails">
            <p className="text-fg-3 text-sm">
              None: it sends as fast as its limits allow.
            </p>
          </FormField>
        )}
      </div>
      <SelectRow
        description="A new mailbox earns its daily limit slowly: it starts at 10% of it, and each clean day raises it until it may use all of it."
        label="Warm-up"
        name="warmup_stage"
        options={WARMUP_OPTIONS}
        problems={panel.problems}
        set={panel.set}
        values={panel.values}
      />
    </PanelForm>
  );
};

const daysOf = (text: string | undefined) =>
  splitList(text)
    .map(Number)
    .filter((day) => WEEKDAYS.some((weekday) => weekday.value === day));

/** The days of the week as joined toggles, Monday first, as the console's segmented control. */
const DayPicker = ({
  onChange,
  value,
}: {
  onChange: (days: number[]) => void;
  value: number[];
}) => (
  <fieldset aria-label="Days" className="flex min-w-0 flex-wrap">
    {WEEKDAYS.map(({ label, value: day }) => {
      const on = value.includes(day);
      return (
        <button
          aria-pressed={on}
          className={cn(
            "border-line text-fg focus-visible:outline-focus disabled:text-fg-4 -ml-px flex h-8 w-12 cursor-pointer items-center justify-center border text-sm transition-colors duration-(--nb-duration-micro) outline-none first:ml-0 first:rounded-l-sm last:rounded-r-sm focus-visible:z-10 focus-visible:outline-1 disabled:cursor-not-allowed",
            on ? "bg-selected font-semibold" : "bg-surface hover:bg-hover"
          )}
          key={day}
          onClick={() =>
            onChange(
              on
                ? value.filter((other) => other !== day)
                : [...value, day].toSorted((a, b) => a - b)
            )
          }
          type="button"
        >
          {label}
        </button>
      );
    })}
  </fieldset>
);

const TIME_INPUT =
  "border-field-line bg-field text-fg hover:border-line-strong focus-visible:border-focus aria-invalid:border-error-line h-8 w-20 rounded-sm border px-3 font-mono text-sm transition-colors duration-(--nb-duration-micro) outline-none";

/** When campaign emails go out: every day and hour, or a window of days and hours, in a zone. */
const WindowPanel = ({ connection, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const panel = usePanel(
    connection,
    (c) => ({
      "send_window.days": (c.send_window?.days ?? [1, 2, 3, 4, 5]).join(","),
      "send_window.end": c.send_window?.end ?? "17:00",
      "send_window.on": c.send_window ? "yes" : "",
      "send_window.start": c.send_window?.start ?? "09:00",
      timezone: c.timezone,
    }),
    (values, base) => {
      const body: UpdateConnection = {};
      if (moved(values, base, "timezone")) {
        body.timezone = values.timezone?.trim();
      }
      const windowMoved = [
        "send_window.on",
        "send_window.days",
        "send_window.start",
        "send_window.end",
      ].some((name) => moved(values, base, name));
      if (windowMoved) {
        body.send_window = values["send_window.on"]
          ? {
              days: daysOf(values["send_window.days"]),
              end: values["send_window.end"]?.trim() ?? "",
              start: values["send_window.start"]?.trim() ?? "",
            }
          : null;
      }
      return workspace.api.connections.update(connection.id, body);
    },
    "Saved: when it sends"
  );
  const { problems, set, values } = panel;
  const windowProblem = problemAt(problems, "send_window");
  return (
    <PanelForm
      description="Campaign emails go out only within these days and hours, read in the mailbox's time zone. Replies you write go out at once."
      fields={["send_window", "timezone"]}
      locked={locked}
      panel={panel}
      title="When it sends"
    >
      <Label className="text-fg gap-2 font-normal">
        <Checkbox
          checked={Boolean(values["send_window.on"])}
          onCheckedChange={(on) => set("send_window.on", on ? "yes" : "")}
        />
        Only send on certain days and hours
      </Label>
      <Reveal gap={4} open={Boolean(values["send_window.on"])}>
        <FormField
          description="Hours on a 24-hour clock, in steps of 5 minutes; 24:00 is midnight."
          label="Days and hours"
          problem={windowProblem}
        >
          <div className="flex flex-wrap items-center gap-3">
            <DayPicker
              onChange={(days) => set("send_window.days", days.join(","))}
              value={daysOf(values["send_window.days"])}
            />
            <span className="flex items-center gap-2">
              <input
                aria-invalid={Boolean(windowProblem)}
                aria-label="Opens at"
                className={TIME_INPUT}
                inputMode="numeric"
                onChange={(event) =>
                  set("send_window.start", event.target.value)
                }
                placeholder="09:00"
                value={values["send_window.start"] ?? ""}
              />
              <span className="text-fg-3 text-sm">to</span>
              <input
                aria-invalid={Boolean(windowProblem)}
                aria-label="Closes at"
                className={TIME_INPUT}
                inputMode="numeric"
                onChange={(event) => set("send_window.end", event.target.value)}
                placeholder="17:00"
                value={values["send_window.end"] ?? ""}
              />
            </span>
          </div>
        </FormField>
      </Reveal>
      <TextRow
        autoComplete="off"
        description="The zone its days and hours are read in, such as Europe/Madrid."
        label="Time zone"
        list="mailbox-time-zones"
        mono
        name="timezone"
        problems={problems}
        set={set}
        values={values}
      />
      <TimeZoneOptions id="mailbox-time-zones" />
    </PanelForm>
  );
};

/** The provider account whose limits the mailbox shares with others (the API's quota scope). */
const ScopePanel = ({ connection, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const required = connection.provider === "ses";
  const panel = usePanel(
    connection,
    (c) => ({ quota_scope_id: c.quota_scope_id ?? "" }),
    (values) =>
      workspace.api.connections.update(connection.id, {
        quota_scope_id: values.quota_scope_id || null,
      }),
    "Saved: shared limit"
  );
  return (
    <PanelForm
      description={
        required
          ? "The SES account and Region whose limits its connections share; an SES connection always names one. The API calls it a quota scope."
          : "When several mailboxes share one provider account's limits (a Google Cloud project, a Microsoft 365 tenant), name that account so Norbelys keeps them under its limits together. The API calls it a quota scope."
      }
      fields={["quota_scope_id"]}
      locked={locked}
      panel={panel}
      title="Shared limit"
    >
      <FormField
        htmlFor={inputId("quota_scope_id")}
        label="Provider account"
        optional={!required}
        problem={problemAt(panel.problems, "quota_scope_id")}
      >
        <ScopeSelect
          disabled={locked}
          id={inputId("quota_scope_id")}
          onChange={(value) => panel.set("quota_scope_id", value)}
          provider={providerInfo(connection.provider)?.id ?? "smtp"}
          required={required}
          value={panel.values.quota_scope_id ?? ""}
        />
      </FormField>
    </PanelForm>
  );
};

const folderNames = (connection: ConnectionObject) =>
  connection.receiving.folders.map((folder) => folder.folder);

/** The folders read for replies: a whole list, replaced only if nobody changed it meanwhile. */
const ReceivingPanel = ({ connection, info, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const panel = usePanel(
    connection,
    (c) => ({ "receiving.folders": folderNames(c).join("\n") }),
    async (values, base) => {
      const outcome = await updateGuarded(
        workspace,
        connection.id,
        (c) => folderNames(c).join("\n"),
        base["receiving.folders"],
        { receiving: { folders: splitList(values["receiving.folders"]) } }
      );
      if (outcome.changed) {
        throw new ChangedError(
          outcome.changed,
          "The folders changed meanwhile, so nothing was saved; here is the latest list. Make your change again."
        );
      }
      return outcome.saved;
    },
    "Saved: the folders read"
  );
  const problem = problemAt(panel.problems, "receiving");
  return (
    <PanelForm
      description="Norbelys reads these folders for replies, bounces and out-of-office answers, at most 10. INBOX is the inbox."
      fields={["receiving"]}
      locked={locked}
      panel={panel}
      title={info?.way === "oauth" ? "Replies" : "Folders read for replies"}
    >
      <FormField
        description="One per line, as the provider names them."
        htmlFor={inputId("receiving.folders")}
        label="Folders"
        problem={problem}
      >
        <Textarea
          aria-invalid={Boolean(problem)}
          className="font-mono"
          id={inputId("receiving.folders")}
          onChange={(event) =>
            panel.set("receiving.folders", event.target.value)
          }
          placeholder="INBOX"
          value={panel.values["receiving.folders"] ?? ""}
        />
      </FormField>
    </PanelForm>
  );
};

/** The body of an IMAP change: new settings (with INBOX read when nothing is), or none. */
const imapBody = (
  connection: ConnectionObject,
  values: Values
): UpdateConnection => {
  if (!values.read_imap) {
    return { imap: null, receiving: { folders: [] } };
  }
  const imap = {
    host: values["imap.host"]?.trim() ?? "",
    port: integer(values, "imap.port") ?? 0,
    security: (values["imap.security"] ?? "tls") as ImapSecurity,
  };
  return connection.receiving.folders.length > 0
    ? { imap }
    : { imap, receiving: { folders: ["INBOX"] } };
};

/** An SMTP login's IMAP server, which reads its replies with the same login and password. */
const ImapPanel = ({ connection, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const panel = usePanel(
    connection,
    (c) => ({
      "imap.host": c.imap?.host ?? "",
      "imap.port": String(c.imap?.port ?? 993),
      "imap.security": c.imap?.security ?? "tls",
      read_imap: c.imap ? "yes" : "",
    }),
    (values) =>
      workspace.api.connections.update(
        connection.id,
        imapBody(connection, values)
      ),
    "Saved: reading replies. The mailbox is checked again."
  );
  const { problems, set, values } = panel;
  return (
    <PanelForm
      description="Norbelys reads replies over IMAP with the same login and password. Saving checks the mailbox again."
      fields={["imap", "receiving"]}
      locked={locked}
      panel={panel}
      title="Replies"
    >
      <Label className="text-fg gap-2 font-normal">
        <Checkbox
          checked={Boolean(values.read_imap)}
          onCheckedChange={(on) => set("read_imap", on ? "yes" : "")}
        />
        Read replies over IMAP
      </Label>
      <Reveal
        className="flex flex-col gap-4"
        gap={4}
        open={Boolean(values.read_imap)}
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
    </PanelForm>
  );
};

const SMTP_FIELDS = [
  "host",
  "port",
  "security",
  "username",
  "configuration_set",
];

/** The SMTP change: the fields that moved, and a new password when one is typed. */
const smtpPatch = (values: Values, base: Values): SmtpPatch => {
  const patch: SmtpPatch = {};
  for (const field of SMTP_FIELDS) {
    if (moved(values, base, `smtp.${field}`)) {
      Object.assign(patch, { [field]: optional(values, `smtp.${field}`) });
    }
  }
  if (patch.port !== undefined) {
    patch.port = integer(values, "smtp.port");
  }
  if (values["smtp.password"]) {
    patch.password = values["smtp.password"];
  }
  return patch;
};

/** An SMTP login's or a relay's server and credential; saving checks the connection again. */
const SmtpPanel = ({ connection, info, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const relay = info?.way === "relay";
  const panel = usePanel(
    connection,
    (c) => ({
      "smtp.configuration_set": c.smtp?.configuration_set ?? "",
      "smtp.host": c.smtp?.host ?? "",
      "smtp.password": "",
      "smtp.port": String(c.smtp?.port ?? 587),
      "smtp.security": c.smtp?.security ?? "starttls",
      "smtp.username": c.smtp?.username ?? "",
    }),
    (values, base) =>
      workspace.api.connections.update(connection.id, {
        smtp: smtpPatch(values, base),
      }),
    "Saved: sign-in. The mailbox is checked again."
  );
  const { problems, set, values } = panel;
  return (
    <PanelForm
      description="The server and password Norbelys sends with. The saved password is never shown; a new one replaces it, and saving checks the mailbox again."
      fields={["smtp"]}
      id="sign-in"
      locked={locked}
      panel={panel}
      title="Sign-in"
    >
      <Endpoint prefix="smtp" problems={problems} set={set} values={values} />
      {relay ? (
        <TextRow
          autoComplete="off"
          label="SMTP login"
          mono
          name="smtp.username"
          problems={problems}
          set={set}
          values={values}
        />
      ) : null}
      <TextRow
        autoComplete="new-password"
        description="Leave it empty to keep the saved one."
        label={
          relay
            ? `New ${info?.smtp?.passwordLabel ?? "password"}`
            : "New password"
        }
        name="smtp.password"
        optional
        problems={problems}
        set={set}
        type="password"
        values={values}
      />
      {connection.provider === "ses" ? (
        <TextRow
          autoComplete="off"
          description="The SES configuration set every message names, so SES publishes its events."
          label="Configuration set"
          mono
          name="smtp.configuration_set"
          problems={problems}
          set={set}
          values={values}
        />
      ) : null}
    </PanelForm>
  );
};

/** A relay's delivery reports: the webhook URL to set at the provider, and the key that signs them. */
const WebhookPanel = ({ connection, info, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const panel = usePanel(
    connection,
    () => ({ "webhook.key": "" }),
    (values) =>
      workspace.api.connections.update(connection.id, {
        webhook: { key: values["webhook.key"]?.trim() ?? "" },
      }),
    "Saved: the webhook key"
  );
  const key = info?.webhookKey;
  if (!key || !connection.webhook) {
    return null;
  }
  const where =
    connection.provider === "ses"
      ? "Subscribe this URL to the SNS topic your configuration set publishes to, over HTTPS. The account's SES connections share it."
      : `Set this URL as ${providerLabel(connection.provider)}'s webhook for delivery events.`;
  return (
    <PanelForm
      description={
        <span className="flex flex-col items-start gap-2">
          {`${providerLabel(connection.provider)} reports what happened to each email (delivered, bounced, complained) to this URL, signed with a key. Norbelys refuses the reports until the key is saved.`}
          <WebhookKeyBadge set={connection.webhook.key_set} />
        </span>
      }
      fields={["webhook"]}
      id="delivery-reports"
      locked={locked}
      panel={panel}
      submitLabel="Save key"
      title="Delivery reports"
    >
      <FormField description={where} label="Webhook URL">
        <CodeLine prefix={null} value={connection.webhook.url} />
      </FormField>
      <TextRow
        autoComplete="off"
        description={`${key.description} A new key replaces the saved one.`}
        label={key.label}
        mono
        name="webhook.key"
        placeholder={key.placeholder}
        problems={panel.problems}
        set={panel.set}
        values={panel.values}
      />
    </PanelForm>
  );
};

/** A relay's API credential: replaced, or removed after a confirmation; never shown. */
const ApiCredentialPanel = ({ connection, info, locked }: PanelProps) => {
  const workspace = useWorkspace();
  const action = useAction();
  const [removing, setRemoving] = useState(false);
  const credential = info?.apiCredential;
  const panel = usePanel(
    connection,
    () => ({ "api_credential.id": "", "api_credential.secret": "" }),
    (values) =>
      workspace.api.connections.update(connection.id, {
        api_credential: {
          id: optional(values, "api_credential.id"),
          secret: values["api_credential.secret"]?.trim() ?? "",
        },
      }),
    "Saved: the API credential. The connection is checked again."
  );
  if (!credential) {
    return null;
  }
  return (
    <PanelForm
      description={`${credential.description} It is stored sealed and never shown, so a saved one cannot be read back here.`}
      fields={["api_credential"]}
      locked={locked}
      panel={panel}
      submitLabel="Save credential"
      title="API credential"
    >
      {credential.idLabel ? (
        <TextRow
          autoComplete="off"
          label={credential.idLabel}
          mono
          name="api_credential.id"
          placeholder="AKIA…"
          problems={panel.problems}
          set={panel.set}
          values={panel.values}
        />
      ) : null}
      <TextRow
        autoComplete="new-password"
        label={credential.secretLabel}
        name="api_credential.secret"
        problems={panel.problems}
        set={panel.set}
        type="password"
        values={panel.values}
      />
      <div>
        <Button
          disabled={locked}
          onClick={() => setRemoving(true)}
          size="s"
          type="button"
          variant="danger-secondary"
        >
          Remove the saved credential
        </Button>
      </div>
      <ConfirmDialog
        confirmLabel="Remove credential"
        danger
        description="Norbelys stops reading the account through its API, and the mailbox is checked again. Sending goes on with the SMTP sign-in."
        onConfirm={() =>
          action(
            "API credential removed",
            () =>
              workspace.api.connections.update(connection.id, {
                api_credential: null,
              }),
            connectionsKey(workspace)
          )
        }
        onOpenChange={setRemoving}
        open={removing}
        title="Remove the saved API credential?"
      />
    </PanelForm>
  );
};

/** What the mailbox sends through: SMTP, or the provider's own API by its name. */
const sendsThrough = (connection: ConnectionObject): string => {
  if (connection.transport !== "api") {
    return "SMTP";
  }
  if (connection.provider === "google") {
    return "The Gmail API";
  }
  return connection.provider === "microsoft"
    ? "Microsoft Graph"
    : "The provider's API";
};

/** The mailbox's facts for whoever works with it through the API: ids, transport, dates. */
const DetailsPanel = ({ connection }: { connection: ConnectionObject }) => (
  <SettingsPanel
    description="For working with this mailbox through the API."
    title="Details"
  >
    <DetailList
      rows={[
        {
          label: "Mailbox ID",
          value: <Copyable mono value={connection.id} />,
        },
        { label: "Account", value: connection.account.email },
        { label: "Provider", value: providerLabel(connection.provider) },
        { label: "Sends through", value: sendsThrough(connection) },
        connection.account.subject
          ? {
              label: "Account ID",
              value: <Copyable mono value={connection.account.subject} />,
            }
          : null,
        connection.quota_scope_id
          ? {
              label: "Quota scope ID",
              value: <Copyable mono value={connection.quota_scope_id} />,
            }
          : null,
        { label: "Connected", value: formatDateTime(connection.created_at) },
        {
          label: "Last checked",
          value: connection.checked_at
            ? formatDateTime(connection.checked_at)
            : "Not yet",
        },
        { label: "Updated", value: formatDateTime(connection.updated_at) },
      ]}
    />
  </SettingsPanel>
);

/** Whether the connection reads folders: a Google or Microsoft mailbox, or a login with IMAP. */
const reads = (connection: ConnectionObject, info: ProviderInfo | undefined) =>
  info?.way === "oauth" || Boolean(connection.imap);

/**
 * A mailbox's settings, one panel each, saved apart with `connections.update`: how much it sends
 * (daily limit, minutes between emails, warm-up), when it sends (days, hours, time zone), its
 * replies (the folders read, an SMTP login's IMAP server), its sign-in (an SMTP server and
 * password), a relay's delivery reports and API credential, the provider account whose limits it
 * shares, and its details for the API. A change of server or credential sends it back to
 * checking. A viewer, or a disconnected mailbox, reads only.
 */
export const MailboxSettings = ({
  connection,
}: {
  connection: ConnectionObject;
}) => {
  const workspace = useWorkspace();
  const info = providerInfo(connection.provider);
  const props = {
    connection,
    info,
    locked: !canChange(workspace, connection),
  };
  return (
    <div className="flex flex-col gap-9">
      <PacePanel {...props} />
      <WindowPanel {...props} />
      {info?.way === "login" ? <ImapPanel {...props} /> : null}
      {reads(connection, info) ? <ReceivingPanel {...props} /> : null}
      {info?.way === "login" || info?.way === "relay" ? (
        <SmtpPanel {...props} />
      ) : null}
      {info?.way === "relay" ? (
        <>
          <WebhookPanel {...props} />
          <ApiCredentialPanel {...props} />
        </>
      ) : null}
      {info?.way === "managed" ? null : <ScopePanel {...props} />}
      <DetailsPanel connection={connection} />
    </div>
  );
};
