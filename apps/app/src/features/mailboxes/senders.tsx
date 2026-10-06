import {
  Add01Icon,
  Alert02Icon,
  PencilEdit02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { APIError } from "@norbelys/sdk";
import type {
  ConnectionObject,
  IdentityInput,
  IdentityObject,
} from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { cn } from "cn";
import { useEffect, useRef, useState } from "react";
import type { ReactNode, RefObject, SubmitEvent } from "react";
import { flushSync } from "react-dom";
import { toast } from "sonner";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyPanel } from "@/components/data-table";
import { Section } from "@/components/page";
import { SaveFailure } from "@/components/problem";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { splitList } from "@/features/mailboxes/form";
import { canChange } from "@/features/mailboxes/mailbox-header";
import {
  connectionQuery,
  connectionsKey,
  updateFromFresh,
} from "@/features/mailboxes/queries";
import { ENTER, Reveal, SmoothHeight } from "@/features/mailboxes/reveal";
import {
  apiHtml,
  cleanSignature,
  signatureLines,
  startingText,
  writtenHtml,
} from "@/features/mailboxes/signature";
import { MailFrame } from "@/features/messages/body-editor";
import { FormField, SwitchField } from "@/lib/form";
import { formatAddress } from "@/lib/format";
import {
  describeProblem,
  fieldProblems,
  problemAt,
  unplacedProblems,
} from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

/** One sender as its form holds it while it is edited. */
interface Draft {
  email: string;
  name: string;
  replyTo: string;
  /** Tags separated by commas. */
  tags: string;
  enabled: boolean;
  /** The person's word that the address may be sent as; `null` keeps what the API holds. */
  attested: boolean | null;
  /** The signature as the person writes it: plain text, line breaks kept. */
  written: string;
  /** The HTML signature set through the API while it is kept; `null` once a written one replaces it. */
  html: string | null;
}

/** A sender's form as it opens: its values, or an empty one for a sender being added. */
const draftOf = (sender: IdentityObject | null): Draft => ({
  attested: sender ? null : false,
  email: sender?.email ?? "",
  enabled: sender?.enabled ?? true,
  html: sender ? apiHtml(sender) : null,
  name: sender?.name ?? "",
  replyTo: sender?.reply_to ?? "",
  tags: sender?.tags.join(", ") ?? "",
  written: sender ? startingText(sender) : "",
});

/**
 * A sender as the API takes it back unchanged: every field given (the API clears an absent one),
 * except `verified`, which it keeps while the address stays.
 */
const inputOf = (sender: IdentityObject): IdentityInput => ({
  email: sender.email,
  enabled: sender.enabled,
  id: sender.id,
  name: sender.name ?? null,
  reply_to: sender.reply_to ?? null,
  signature_html: sender.signature_html ?? null,
  signature_text: sender.signature_text ?? null,
  tags: sender.tags,
});

/**
 * The form as the API takes it. A written signature is sent as text alone, so the server derives
 * the HTML part from it; a kept HTML signature is sent back as it was, with its text part.
 */
const inputFrom = (
  draft: Draft,
  origin: IdentityObject | null
): IdentityInput => ({
  email: draft.email.trim(),
  enabled: draft.enabled,
  id: origin?.id,
  name: draft.name.trim() || null,
  reply_to: draft.replyTo.trim() || null,
  signature_html: draft.html,
  signature_text:
    draft.html === null
      ? cleanSignature(draft.written)
      : (origin?.signature_text ?? null),
  tags: splitList(draft.tags),
  verified: draft.attested ?? undefined,
});

/** Whether two reads of a sender hold the same values (its `verified` aside, which checks move). */
const sameSender = (a: IdentityObject, b: IdentityObject) =>
  JSON.stringify(inputOf(a)) === JSON.stringify(inputOf(b));

/** Shows a connection the API answered at once, and has every list of connections read again. */
const useShowConnection = () => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  return (connection: ConnectionObject) => {
    queryClient.setQueryData(
      connectionQuery(workspace, connection.id).queryKey,
      connection
    );
    void queryClient.invalidateQueries({ queryKey: connectionsKey(workspace) });
  };
};

/** What moved under a form while it was open: its sender changed, or was removed. */
type Moved = "changed" | "removed";

/**
 * Closes a form and gives the keyboard back to the button that opened it: the closing state is
 * committed at once (so that button is back on the page), then the button takes the focus the
 * form's own buttons held.
 */
const closeTo = (
  close: () => void,
  opener: RefObject<HTMLButtonElement | null>
) => {
  flushSync(close);
  opener.current?.focus({ preventScroll: true });
};

/**
 * One sender's form: its draft, and its save. The whole list is replaced on save (the API's
 * way), built from a fresh read with only this sender changed, so a change made meanwhile to
 * another sender is kept. When this sender itself changed meanwhile, nothing is saved and the
 * person decides: saving again keeps their version.
 */
const useSenderForm = (
  connection: ConnectionObject,
  sender: IdentityObject | null,
  onSaved: (saved: ConnectionObject) => void
) => {
  const workspace = useWorkspace();
  const showConnection = useShowConnection();
  const [origin, setOrigin] = useState(sender);
  const [draft, setDraft] = useState(() => draftOf(sender));
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  // The sender's place in the list the API was sent, where its field problems point.
  const [index, setIndex] = useState(0);
  const [moved, setMoved] = useState<Moved | null>(null);
  const dirty =
    JSON.stringify(inputFrom(draft, origin)) !==
    JSON.stringify(inputFrom(draftOf(origin), origin));

  const save = async () => {
    setSaving(true);
    setFailure(null);
    setMoved(null);
    let at = 0;
    try {
      const outcome = await updateFromFresh(
        workspace,
        connection.id,
        (fresh) => {
          if (origin === null) {
            at = fresh.identities.length;
            return {
              identities: [
                ...fresh.identities.map(inputOf),
                inputFrom(draft, null),
              ],
            };
          }
          const current = fresh.identities.find(
            (identity) => identity.id === origin.id
          );
          if (!current || !sameSender(current, origin)) {
            return null;
          }
          at = fresh.identities.indexOf(current);
          return {
            identities: fresh.identities.map((identity) =>
              identity.id === origin.id
                ? inputFrom(draft, origin)
                : inputOf(identity)
            ),
          };
        }
      );
      if (outcome.saved) {
        showConnection(outcome.saved);
        toast.success(origin ? "Sender saved" : "Sender added");
        onSaved(outcome.saved);
      } else {
        showConnection(outcome.changed);
        const current =
          outcome.changed.identities.find(
            (identity) => identity.id === origin?.id
          ) ?? null;
        setOrigin(current);
        setMoved(current ? "changed" : "removed");
      }
    } catch (error) {
      setIndex(at);
      setFailure(error);
    }
    setSaving(false);
  };

  return {
    dirty,
    draft,
    failure,
    index,
    moved,
    origin,
    save,
    saving,
    set: (patch: Partial<Draft>) =>
      setDraft((current) => ({ ...current, ...patch })),
  };
};

type SenderForm = ReturnType<typeof useSenderForm>;

/** What a written signature looks like before anyone writes one. */
const SIGNATURE_PLACEHOLDER = "Ada Lovelace\nFounder, Analytical Engines";

/**
 * Where the email's own text goes in a preview: three grey lines, so only the signature reads.
 * The lines live in the mail's document, on the mail client's white, not in the dashboard.
 */
const PREVIEW_BODY =
  '<style>.nb-lines i{display:block;height:7px;margin:0 0 10px;border-radius:4px;background:#e9e9ec}</style><div class="nb-lines" aria-hidden="true"><i style="width:92%"></i><i style="width:84%"></i><i style="width:46%"></i></div>';

/** The end of an email, with the signature wrapped exactly as the server adds it. */
const previewHtml = (signature: string | null): string =>
  signature === null
    ? PREVIEW_BODY
    : `${PREVIEW_BODY}<div style="margin-top:16px">${signature}</div>`;

/**
 * How an email from the sender ends, as its recipient sees it: who it is from, the place of the
 * text, then the signature. It renders in the mail preview's frame, which runs no script, so an
 * HTML signature set through the API shows as mail clients draw it.
 */
const SenderPreview = ({ draft }: { draft: Draft }) => {
  const email = draft.email.trim();
  return (
    <figure className="flex min-w-0 flex-col gap-1.5 self-start">
      <figcaption className="text-fg-2 text-sm font-semibold">
        How it looks
      </figcaption>
      <div className="border-line flex min-w-0 flex-col overflow-hidden rounded-lg border">
        <p className="border-line flex min-w-0 gap-3 border-b px-4 py-2 text-xs">
          <span className="text-fg-3">From</span>
          <span className="text-fg-2 min-w-0 truncate">
            {email
              ? formatAddress({ email, name: draft.name.trim() || null })
              : "No address yet"}
          </span>
        </p>
        {/* Tall enough for a signature of four lines, so the frame keeps its size as it loads. */}
        <MailFrame
          className="h-52 rounded-none border-0"
          html={previewHtml(draft.html ?? writtenHtml(draft.written))}
          title="Preview: the end of an email from this sender"
        />
      </div>
    </figure>
  );
};

/**
 * The one signature field: a text area where the person writes the signature as it should look,
 * beside the preview of an email's end. A signature set as HTML through the API shows instead as
 * a note with a way to write a new one (which replaces it on save), and back.
 */
const SignatureField = ({
  form,
  id,
  problem,
}: {
  form: SenderForm;
  id: string;
  problem: string | undefined;
}) => {
  const { draft, origin, set } = form;
  const kept = origin ? apiHtml(origin) : null;
  // Set once the person switches, so the switch fades in but the form's opening does not.
  const [switched, setSwitched] = useState(false);
  const writeButton = useRef<HTMLButtonElement>(null);
  // Each switch hands the keyboard to what replaced the control that was pressed.
  const write = () => {
    flushSync(() => {
      setSwitched(true);
      set({ html: null });
    });
    document
      .querySelector<HTMLTextAreaElement>(`#${id}`)
      ?.focus({ preventScroll: true });
  };
  const keep = () => {
    flushSync(() => {
      setSwitched(true);
      set({ html: kept });
    });
    writeButton.current?.focus({ preventScroll: true });
  };
  return (
    <div className="grid gap-x-6 gap-y-4 md:col-span-2 md:grid-cols-2">
      <SmoothHeight>
        <div
          className={cn(switched && ENTER)}
          key={draft.html === null ? "written" : "html"}
        >
          {draft.html === null ? (
            <FormField
              description={
                kept === null ? (
                  "Write it as it should look under your emails. Line breaks are kept."
                ) : (
                  <>
                    Saving replaces the HTML signature.{" "}
                    <button
                      className="text-link hover:text-link-hover focus-visible:outline-focus cursor-pointer rounded-xs font-semibold outline-none focus-visible:outline-1"
                      onClick={keep}
                      type="button"
                    >
                      Keep the HTML one
                    </button>
                  </>
                )
              }
              htmlFor={id}
              label="Signature"
              optional
              problem={problem}
            >
              <Textarea
                aria-invalid={Boolean(problem)}
                className="min-h-32"
                id={id}
                onChange={(event) => set({ written: event.target.value })}
                placeholder={SIGNATURE_PLACEHOLDER}
                value={draft.written}
              />
            </FormField>
          ) : (
            <FormField label="Signature">
              <div className="border-line flex flex-col items-start gap-3 rounded-sm border p-3">
                <p className="text-fg-2 text-sm">
                  This signature was set as HTML through the API. It ends every
                  email from this sender as the preview shows.
                </p>
                <Button
                  onClick={write}
                  ref={writeButton}
                  size="s"
                  type="button"
                  variant="secondary"
                >
                  <HugeiconsIcon icon={PencilEdit02Icon} />
                  Write a new signature
                </Button>
              </div>
            </FormField>
          )}
        </div>
      </SmoothHeight>
      <SenderPreview draft={draft} />
    </div>
  );
};

/** Why the form was not saved: the sender changed or was removed while it was open. */
const MovedNotice = ({ moved }: { moved: Moved }) => (
  <Alert variant="warning">
    <HugeiconsIcon icon={Alert02Icon} />
    <AlertDescription>
      {moved === "changed"
        ? "Someone changed this sender while you were editing it, so nothing was saved. Save again to keep your version, or cancel to see theirs."
        : "This sender was removed while you were editing it, so nothing was saved. Saving adds it again."}
    </AlertDescription>
  </Alert>
);

/** The list without `sender`, built from a fresh read so another change made meanwhile stays. */
const removeSender = async (
  workspace: Workspace,
  connection: ConnectionObject,
  sender: IdentityObject
): Promise<ConnectionObject> => {
  const outcome = await updateFromFresh(workspace, connection.id, (fresh) => ({
    identities: fresh.identities
      .filter((identity) => identity.id !== sender.id)
      .map(inputOf),
  }));
  return outcome.saved ?? outcome.changed;
};

/**
 * Removes a sender, asking first. The API refuses to remove a sender that has sent mail (its
 * history points at it, `invalid_state`): that refusal is said in plain words with what to do
 * instead; any other failure in the API's own words.
 */
const RemoveSender = ({
  connection,
  sender,
}: {
  connection: ConnectionObject;
  sender: IdentityObject;
}) => {
  const workspace = useWorkspace();
  const showConnection = useShowConnection();
  const [open, setOpen] = useState(false);
  const remove = async () => {
    try {
      showConnection(await removeSender(workspace, connection, sender));
      toast.success("Sender removed");
    } catch (error) {
      toast.error(
        error instanceof APIError && error.code === "invalid_state"
          ? "This sender has sent mail, so it stays for its history. Turn off Use in campaigns instead."
          : describeProblem(error).detail
      );
    }
  };
  return (
    <>
      <Button
        className="ml-auto"
        onClick={() => setOpen(true)}
        type="button"
        variant="danger-secondary"
      >
        Remove sender
      </Button>
      <ConfirmDialog
        confirmLabel="Remove sender"
        danger
        description="Campaigns stop sending as this address. A sender that has already sent mail stays for its history and can't be removed: turn off Use in campaigns instead."
        onConfirm={remove}
        onOpenChange={setOpen}
        open={open}
        title={`Remove ${sender.email}?`}
      />
    </>
  );
};

/**
 * A sender's form, opened in place: its address and name, its one signature beside a preview of
 * an email's end, where replies go, its tags, whether campaigns use it, and the person's word
 * when nobody confirmed the address yet. Save sends it; Cancel closes it as it was.
 */
const SenderEditor = ({
  connection,
  onClose,
  onSaved,
  sender,
}: {
  connection: ConnectionObject;
  onClose: () => void;
  onSaved: (saved: ConnectionObject) => void;
  sender: IdentityObject | null;
}) => {
  const form = useSenderForm(connection, sender, onSaved);
  const { draft, set } = form;
  const problems = fieldProblems(form.failure);
  const place = `identities[${form.index}]`;
  const at = (field: string) => problemAt(problems, `${place}.${field}`);
  const id = (field: string) => `sender-${sender?.id ?? "new"}-${field}`;
  const ready =
    draft.email.trim() !== "" && (form.dirty || form.moved === "removed");
  // The form opens where the person clicked: the keyboard follows it to its first field.
  const first = useRef<HTMLInputElement>(null);
  useEffect(() => {
    first.current?.focus({ preventScroll: true });
  }, []);
  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    void form.save();
  };
  return (
    <form className="flex flex-col gap-5 px-4 pt-3 pb-4" onSubmit={submit}>
      <fieldset
        className="grid min-w-0 gap-x-6 gap-y-5 md:grid-cols-2"
        disabled={form.saving}
      >
        <FormField htmlFor={id("email")} label="Address" problem={at("email")}>
          <Input
            aria-invalid={Boolean(at("email"))}
            autoComplete="off"
            id={id("email")}
            onChange={(event) => set({ email: event.target.value })}
            placeholder="ada@example.com"
            readOnly={
              connection.provider === "norbelys" &&
              connection.account.email.includes("@")
            }
            ref={first}
            required
            type="email"
            value={draft.email}
          />
        </FormField>
        <FormField
          description="The name people see beside the address."
          htmlFor={id("name")}
          label="Name"
          optional
          problem={at("name")}
        >
          <Input
            aria-invalid={Boolean(at("name"))}
            autoComplete="off"
            id={id("name")}
            onChange={(event) => set({ name: event.target.value })}
            placeholder="Ada Lovelace"
            value={draft.name}
          />
        </FormField>
        <SignatureField
          form={form}
          id={id("signature")}
          problem={at("signature_text") ?? at("signature_html")}
        />
        <FormField
          description="Where replies go, when not to the address itself."
          htmlFor={id("reply-to")}
          label="Reply-To"
          optional
          problem={at("reply_to")}
        >
          <Input
            aria-invalid={Boolean(at("reply_to"))}
            autoComplete="off"
            id={id("reply-to")}
            onChange={(event) => set({ replyTo: event.target.value })}
            placeholder="replies@example.com"
            type="email"
            value={draft.replyTo}
          />
        </FormField>
        <FormField
          description="Campaigns can choose their senders by tag. Separate tags with commas."
          htmlFor={id("tags")}
          label="Tags"
          optional
          problem={at("tags")}
        >
          <Input
            aria-invalid={Boolean(at("tags"))}
            autoComplete="off"
            id={id("tags")}
            onChange={(event) => set({ tags: event.target.value })}
            placeholder="sales, europe"
            value={draft.tags}
          />
        </FormField>
        <div className="flex flex-col gap-3 md:col-span-2">
          <SwitchField
            checked={draft.enabled}
            description="When it is off, campaigns don't send from this address."
            id={id("enabled")}
            label="Use in campaigns"
            onChange={(enabled) => set({ enabled })}
          />
          {sender?.verified ||
          (connection.provider === "norbelys" &&
            !connection.account.email.includes("@")) ? null : (
            <Label className="text-fg gap-3 font-normal">
              <Checkbox
                checked={draft.attested ?? false}
                onCheckedChange={(attested) => set({ attested })}
              />
              I confirm this mailbox may send as this address
            </Label>
          )}
        </div>
      </fieldset>
      {form.moved ? <MovedNotice moved={form.moved} /> : null}
      <SaveFailure
        failure={form.failure}
        unplaced={unplacedProblems(problems, [place])}
      />
      <div className="flex flex-wrap items-center gap-2">
        <Button
          disabled={!ready || form.saving}
          type="submit"
          variant="primary"
        >
          {form.saving ? <Spinner /> : null}
          {sender ? "Save sender" : "Add sender"}
        </Button>
        <Button
          disabled={form.saving}
          onClick={onClose}
          type="button"
          variant="tertiary"
        >
          Cancel
        </Button>
        {sender &&
        (connection.provider !== "norbelys" ||
          !connection.account.email.includes("@")) ? (
          <RemoveSender connection={connection} sender={sender} />
        ) : null}
      </div>
    </form>
  );
};

/** Where replies go and the tags of a sender, when it has them, in one quiet line. */
const senderMeta = (sender: IdentityObject): string | null => {
  const parts = [
    sender.reply_to ? `Replies go to ${sender.reply_to}` : null,
    sender.tags.length > 0 ? `Tags: ${sender.tags.join(", ")}` : null,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : null;
};

/** A sender as the list reads it: its signature's lines (or that it has none) and its details. */
const SenderSummary = ({ sender }: { sender: IdentityObject }) => {
  const lines = signatureLines(sender);
  const meta = senderMeta(sender);
  return (
    <div className="flex flex-col gap-2 px-4 pt-2 pb-3.5">
      {lines ? (
        <div className="border-line flex items-start gap-2 border-l pl-3">
          <p className="text-fg-2 line-clamp-6 min-w-0 text-sm whitespace-pre-line">
            {lines}
          </p>
          {apiHtml(sender) === null ? null : (
            <Badge title="Set as HTML through the API">HTML</Badge>
          )}
        </div>
      ) : (
        <p className="text-fg-3 text-sm">No signature</p>
      )}
      {meta ? <p className="text-fg-3 text-xs">{meta}</p> : null}
    </div>
  );
};

/** A sender's first line: the name people see and the address, with what needs a look. */
const SenderHeading = ({
  action,
  sender,
}: {
  action: ReactNode;
  sender: IdentityObject;
}) => (
  <div className="flex items-start gap-3 px-4 pt-3.5">
    <div className="flex min-w-0 flex-1 flex-col">
      <span className="text-fg truncate font-semibold">
        {sender.name || sender.email}
      </span>
      {sender.name ? (
        <span className="text-fg-3 truncate text-xs">{sender.email}</span>
      ) : null}
    </div>
    <div className="flex shrink-0 items-center gap-2">
      {sender.enabled ? null : (
        <Badge tone="muted" title="Campaigns don't send from this address">
          Not in campaigns
        </Badge>
      )}
      {sender.verified ? null : (
        <Badge
          title="Neither the provider nor a person has confirmed this mailbox may send as this address"
          tone="warning"
        >
          Not confirmed
        </Badge>
      )}
      {action}
    </div>
  </div>
);

/** One sender: read, or open in place as its form. */
const SenderRow = ({
  connection,
  editable,
  sender,
}: {
  connection: ConnectionObject;
  editable: boolean;
  sender: IdentityObject;
}) => {
  const [editing, setEditing] = useState(false);
  // Set by the first opening, so what replaces the summary fades in but a page's load does not.
  const [touched, setTouched] = useState(false);
  const editButton = useRef<HTMLButtonElement>(null);
  const close = () => closeTo(() => setEditing(false), editButton);
  return (
    <div className="flex flex-col">
      <SenderHeading
        action={
          editable && !editing ? (
            <Button
              aria-label={`Edit ${sender.email}`}
              onClick={() => {
                setTouched(true);
                setEditing(true);
              }}
              ref={editButton}
              size="s"
              variant="tertiary"
            >
              <HugeiconsIcon icon={PencilEdit02Icon} />
              Edit
            </Button>
          ) : null
        }
        sender={sender}
      />
      <SmoothHeight>
        <div
          className={cn(touched && ENTER)}
          key={editing ? "form" : "summary"}
        >
          {editing ? (
            <SenderEditor
              connection={connection}
              onClose={close}
              onSaved={close}
              sender={sender}
            />
          ) : (
            <SenderSummary sender={sender} />
          )}
        </div>
      </SmoothHeight>
    </div>
  );
};

const senderDescription = (connection: ConnectionObject): string => {
  if (connection.provider === "ses" && connection.send_interval_minutes) {
    return "A paced SES connection sends as one address: its account's.";
  }
  if (["google", "microsoft", "smtp"].includes(connection.provider)) {
    return "Your mailbox's sender is created automatically. Add an alias only if your mail provider allows it; aliases share the mailbox's limit and pace.";
  }
  if (connection.provider === "norbelys") {
    return connection.account.email.includes("@")
      ? "This mailbox has its own sender and incoming mail. Manage domain senders from Norbelys mail."
      : "Send from any address on this verified domain with your workspace API key. Add senders here to manage their names and signatures; sending from a new address through the API adds it automatically.";
  }
  return "The addresses authorized by this service, with their names and signatures. They share the account's sending limits.";
};

/**
 * A mailbox's senders: the From addresses its mail goes out as, each with the name people see and
 * the one signature under its emails. They share the mailbox's daily limit and pace, so a sender
 * added never adds volume. A sender opens in place to be edited; a new one opens at the end of
 * the list. A viewer, or a disconnected mailbox, reads only.
 */
export const Senders = ({ connection }: { connection: ConnectionObject }) => {
  const workspace = useWorkspace();
  const editable = canChange(workspace, connection);
  const [adding, setAdding] = useState(false);
  // Set by the first Add, so the empty state's form fades in but a page's load does not.
  const [touched, setTouched] = useState(false);
  // The sender just added fades in where its form was.
  const [added, setAdded] = useState<string | null>(null);
  const addButton = useRef<HTMLButtonElement>(null);
  const { identities } = connection;
  const paced =
    connection.provider === "ses" && Boolean(connection.send_interval_minutes);
  const mailbox = ["google", "microsoft", "smtp"].includes(connection.provider);
  const canAdd =
    editable &&
    !paced &&
    (connection.provider !== "norbelys" ||
      !connection.account.email.includes("@"));
  const addLabel = mailbox ? "Add alias" : "Add sender";
  const editor = (
    <SenderEditor
      connection={connection}
      onClose={() => closeTo(() => setAdding(false), addButton)}
      onSaved={(saved) =>
        closeTo(() => {
          setAdded(
            saved.identities.find(
              (identity) =>
                !identities.some((other) => other.id === identity.id)
            )?.id ?? null
          );
          setAdding(false);
        }, addButton)
      }
      sender={null}
    />
  );
  const add = () => {
    setTouched(true);
    setAdding(true);
  };
  return (
    <Section
      actions={
        // With no sender at all, the empty state holds the one Add button.
        canAdd && identities.length > 0 ? (
          <Button
            disabled={adding}
            onClick={add}
            ref={addButton}
            size="s"
            variant="secondary"
          >
            <HugeiconsIcon icon={Add01Icon} />
            {addLabel}
          </Button>
        ) : null
      }
      title="Senders"
    >
      <p className="text-fg-2 mb-1 text-sm">{senderDescription(connection)}</p>
      {connection.provider === "norbelys" &&
      !connection.account.email.includes("@") ? (
        <p className="text-fg-2 mb-3 text-sm">
          SMTP: {connection.smtp?.host ?? "your installation's SMTP host"}, port
          587 with STARTTLS. Username: {connection.account.email}. Password:
          your existing live API key from Developers → API keys, with permission
          to send messages. The same key works with the HTTP API.
        </p>
      ) : null}
      {identities.length > 0 ? (
        <div className="border-line rounded-sm border">
          <ul>
            {identities.map((identity, index) => (
              <li
                className={cn(
                  index > 0 && "border-line border-t",
                  identity.id === added && ENTER
                )}
                key={identity.id}
              >
                <SenderRow
                  connection={connection}
                  editable={editable}
                  sender={identity}
                />
              </li>
            ))}
          </ul>
          <Reveal open={adding}>
            <div className="border-line border-t">
              <p className="text-fg px-4 pt-3.5 font-semibold">New sender</p>
              {editor}
            </div>
          </Reveal>
        </div>
      ) : (
        <SmoothHeight>
          <div className={cn(touched && ENTER)} key={adding ? "form" : "empty"}>
            {adding ? (
              <div className="border-line rounded-sm border">
                <p className="text-fg px-4 pt-3.5 font-semibold">New sender</p>
                {editor}
              </div>
            ) : (
              <EmptyPanel
                action={
                  canAdd ? (
                    <Button onClick={add} ref={addButton} variant="secondary">
                      <HugeiconsIcon icon={Add01Icon} />
                      {addLabel}
                    </Button>
                  ) : null
                }
                description="Campaigns can't send from this mailbox until it has a sender: the address and name people see."
                icon={Add01Icon}
                illustration="mailbox"
                title="No senders yet"
              />
            )}
          </div>
        </SmoothHeight>
      )}
    </Section>
  );
};
