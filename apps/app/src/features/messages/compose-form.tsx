import type { ConnectionObject, CreateMessage } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { ProblemAlert } from "@/components/problem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import type { SelectOption } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { mailboxOptionsQuery } from "@/features/mailboxes/queries";
import {
  BodyEditor,
  bodyHtml,
  EMPTY_BODY,
} from "@/features/messages/body-editor";
import type { BodyDraft } from "@/features/messages/body-editor";
import { messagesKey } from "@/features/messages/queries";
import { FormField } from "@/lib/form";
import { formatAddress, fromLocalInput } from "@/lib/format";
import { fieldProblems, problemAt, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** Addresses typed as a list: separated by commas, semicolons, spaces or lines. */
const addresses = (text: string): string[] =>
  text
    .split(/[\s,;]+/u)
    .map((address) => address.trim())
    .filter(Boolean);

/**
 * The identities a message can be sent from: the enabled identities of mailboxes that are not
 * archived, each once by address (the API takes the address as `from`).
 */
const senderOptions = (
  mailboxes: readonly ConnectionObject[]
): SelectOption[] => {
  const seen = new Set<string>();
  const options: SelectOption[] = [];
  for (const mailbox of mailboxes) {
    if (mailbox.status === "archived") {
      continue;
    }
    for (const identity of mailbox.identities) {
      const key = identity.email;
      if (identity.enabled && !seen.has(key)) {
        seen.add(key);
        const name = formatAddress(identity);
        options.push({
          label: mailbox.paused ? `${name} (paused)` : name,
          value: identity.email,
        });
      }
    }
  }
  return options;
};

/** What the form holds while it is written. */
interface Draft {
  from: string | null;
  to: string;
  cc: string;
  bcc: string;
  subject: string;
  body: BodyDraft;
  sendAt: string;
  expiresAt: string;
  variables: string;
}

const EMPTY_DRAFT: Draft = {
  bcc: "",
  body: EMPTY_BODY,
  cc: "",
  expiresAt: "",
  from: null,
  sendAt: "",
  subject: "",
  to: "",
  variables: "",
};

/**
 * The variables as the object the API takes, or why they are not one: checked while they are
 * typed, so a request never leaves with them broken.
 */
const parseVariables = (
  text: string
): { value?: Record<string, unknown>; problem?: string } => {
  if (!text.trim()) {
    return {};
  }
  let parsed: unknown = null;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { problem: "This is not valid JSON." };
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return { problem: 'Variables are a JSON object, such as {"name": "Ada"}.' };
  }
  return { value: parsed as Record<string, unknown> };
};

/** The body of `messages.create` for a direct message, from the draft. */
const toRequest = (draft: Draft): CreateMessage => {
  const cc = addresses(draft.cc);
  const bcc = addresses(draft.bcc);
  return {
    bcc: bcc.length > 0 ? bcc : undefined,
    cc: cc.length > 0 ? cc : undefined,
    expires_at: fromLocalInput(draft.expiresAt),
    from: draft.from ?? "",
    html: bodyHtml(draft.body),
    send_at: fromLocalInput(draft.sendAt),
    subject: draft.subject.trim(),
    to: addresses(draft.to),
    variables: parseVariables(draft.variables).value,
  };
};

/** No identity can send: the mailboxes page is where one is connected. */
const NoSender = () => {
  const workspace = useWorkspace();
  return (
    <Alert variant="warning">
      <AlertTitle>No sender yet</AlertTitle>
      <AlertDescription>
        A message is sent from an enabled identity of one of your mailboxes.{" "}
        <Link params={{ slug: workspace.slug }} to="/w/$slug/mailboxes/new">
          Connect a mailbox
        </Link>{" "}
        first.
      </AlertDescription>
    </Alert>
  );
};

/** The recipients: `to` required, `cc` and `bcc` side by side. */
const RecipientFields = ({
  draft,
  fields,
  set,
}: {
  draft: Draft;
  fields: Readonly<Record<string, string>>;
  set: (patch: Partial<Draft>) => void;
}) => (
  <>
    <FormField
      description="1 to 50 addresses, separated by commas. At most 150 recipients in all, with Cc and Bcc."
      htmlFor="compose-to"
      label="To"
      problem={problemAt(fields, "to")}
    >
      <Input
        aria-invalid={Boolean(problemAt(fields, "to"))}
        id="compose-to"
        onChange={(event) => set({ to: event.target.value })}
        placeholder="ada@example.com, grace@example.com"
        value={draft.to}
      />
    </FormField>
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField
        htmlFor="compose-cc"
        label="Cc"
        optional
        problem={problemAt(fields, "cc")}
      >
        <Input
          aria-invalid={Boolean(problemAt(fields, "cc"))}
          id="compose-cc"
          onChange={(event) => set({ cc: event.target.value })}
          value={draft.cc}
        />
      </FormField>
      <FormField
        htmlFor="compose-bcc"
        label="Bcc"
        optional
        problem={problemAt(fields, "bcc")}
      >
        <Input
          aria-invalid={Boolean(problemAt(fields, "bcc"))}
          id="compose-bcc"
          onChange={(event) => set({ bcc: event.target.value })}
          value={draft.bcc}
        />
      </FormField>
    </div>
  </>
);

/** When it is sent and until when it is still worth sending. */
const ScheduleFields = ({
  draft,
  fields,
  set,
}: {
  draft: Draft;
  fields: Readonly<Record<string, string>>;
  set: (patch: Partial<Draft>) => void;
}) => (
  <div className="grid gap-4 sm:grid-cols-2">
    <FormField
      description="At most 7 days ahead. Empty: as soon as the mailbox's pacing allows."
      htmlFor="compose-send-at"
      label="Send at"
      optional
      problem={problemAt(fields, "send_at")}
    >
      <Input
        aria-invalid={Boolean(problemAt(fields, "send_at"))}
        id="compose-send-at"
        onChange={(event) => set({ sendAt: event.target.value })}
        type="datetime-local"
        value={draft.sendAt}
      />
    </FormField>
    <FormField
      description="Past this time it is no longer sent."
      htmlFor="compose-expires-at"
      label="Expires at"
      optional
      problem={problemAt(fields, "expires_at")}
    >
      <Input
        aria-invalid={Boolean(problemAt(fields, "expires_at"))}
        id="compose-expires-at"
        onChange={(event) => set({ expiresAt: event.target.value })}
        type="datetime-local"
        value={draft.expiresAt}
      />
    </FormField>
  </div>
);

const PLACED = [
  "from",
  "to",
  "cc",
  "bcc",
  "subject",
  "html",
  "send_at",
  "expires_at",
  "variables",
];

/**
 * Writes a direct message (`messages.create`): a sender identity, recipients, a subject and one
 * HTML body, optionally scheduled and given an expiry. The message is queued at once and sent when
 * due, within the mailbox's pacing, outside any campaign; on success the new message opens.
 */
export const ComposeForm = () => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const mailboxes = useQuery(mailboxOptionsQuery(workspace));
  const senders = senderOptions(mailboxes.data?.data ?? []);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const fields = fieldProblems(failure);
  const unplaced = failure && !PLACED.some((name) => problemAt(fields, name));
  const set = (patch: Partial<Draft>) =>
    setDraft((current) => ({ ...current, ...patch }));
  const variables = parseVariables(draft.variables);
  const ready =
    [
      draft.from ?? "",
      draft.to.trim(),
      draft.subject.trim(),
      bodyHtml(draft.body),
    ].every(Boolean) && !variables.problem;

  const submit = async () => {
    setBusy(true);
    setFailure(null);
    try {
      const created = await workspace.api.messages.create(toRequest(draft));
      await queryClient.invalidateQueries({ queryKey: messagesKey(workspace) });
      toast.success("Message queued");
      if ("id" in created) {
        void navigate({
          params: { messageId: created.id, slug: workspace.slug },
          to: "/w/$slug/messages/$messageId",
        });
      }
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  if (mailboxes.isSuccess && senders.length === 0) {
    return <NoSender />;
  }
  return (
    <form
      className="flex max-w-[800px] flex-col gap-5"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <FormField
        description="One of your mailboxes' enabled identities."
        htmlFor="compose-from"
        label="From"
        problem={problemAt(fields, "from")}
      >
        <Select
          disabled={mailboxes.isPending}
          id="compose-from"
          onChange={(from) => set({ from })}
          options={senders}
          placeholder={
            mailboxes.isPending ? "Loading senders…" : "Choose a sender"
          }
          value={draft.from}
        />
      </FormField>
      <RecipientFields draft={draft} fields={fields} set={set} />
      <FormField
        description="A template: {{ person.given_name }} when a recipient is one of your people, {{ sender.name }}, {{ variables.… }}."
        htmlFor="compose-subject"
        label="Subject"
        problem={problemAt(fields, "subject")}
      >
        <Input
          aria-invalid={Boolean(problemAt(fields, "subject"))}
          id="compose-subject"
          maxLength={1000}
          onChange={(event) => set({ subject: event.target.value })}
          value={draft.subject}
        />
      </FormField>
      <FormField
        description="One HTML body; the plain-text version is derived from it. Plain text is sent as paragraphs."
        htmlFor="compose-body"
        label="Body"
        problem={problemAt(fields, "html")}
      >
        <BodyEditor
          draft={draft.body}
          id="compose-body"
          invalid={Boolean(problemAt(fields, "html"))}
          onChange={(body) => set({ body })}
          placeholder='Hi {{ person.given_name | default("there") }},'
        />
      </FormField>
      <ScheduleFields draft={draft} fields={fields} set={set} />
      <FormField
        description='A JSON object the templates read as {{ variables.name }}, such as {"name": "Ada"}.'
        htmlFor="compose-variables"
        label="Variables"
        optional
        problem={variables.problem ?? problemAt(fields, "variables")}
      >
        <Textarea
          aria-invalid={Boolean(
            variables.problem ?? problemAt(fields, "variables")
          )}
          className="font-mono text-xs"
          id="compose-variables"
          onChange={(event) => set({ variables: event.target.value })}
          placeholder="{}"
          spellCheck={false}
          value={draft.variables}
        />
      </FormField>
      {unplaced ? <ProblemAlert>{problemLine(failure)}</ProblemAlert> : null}
      <div className="border-line flex items-center justify-end gap-2 border-t pt-5">
        <Button
          nativeButton={false}
          render={
            <Link
              aria-label="Cancel"
              params={{ slug: workspace.slug }}
              to="/w/$slug/messages"
            />
          }
          variant="secondary"
        >
          Cancel
        </Button>
        <Button disabled={busy || !ready} type="submit" variant="primary">
          {busy ? <Spinner /> : null}
          {draft.sendAt ? "Schedule message" : "Send message"}
        </Button>
      </div>
    </form>
  );
};
