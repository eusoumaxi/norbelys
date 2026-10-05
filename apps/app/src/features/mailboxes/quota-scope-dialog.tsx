import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { Provider, QuotaScopeObject, WindowUnit } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import type { SubmitEvent } from "react";
import { useState } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { SaveFailure } from "@/components/problem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  integer,
  SelectRow,
  TextRow,
  useValues,
} from "@/features/mailboxes/form";
import type { Values } from "@/features/mailboxes/form";
import { holdUntil, useNow } from "@/features/mailboxes/parts";
import {
  PROVIDER_IDS,
  PROVIDERS,
  providerInfo,
  providerLabel,
} from "@/features/mailboxes/providers";
import { quotaScopesKey } from "@/features/mailboxes/queries";
import { formatTimestamp } from "@/lib/format";
import { fieldProblems, unplacedProblems } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

const PROVIDER_OPTIONS = PROVIDER_IDS.map((id) => ({
  label: PROVIDERS[id].label,
  value: id,
}));

const UNIT_OPTIONS: { label: string; value: WindowUnit }[] = [
  { label: "Recipients", value: "recipients" },
  { label: "Requests", value: "requests" },
  { label: "Units", value: "units" },
];

const FIELDS = [
  "provider",
  "scope_key",
  "messages_per_day",
  "recipients_per_day",
  "window_limit",
  "window_unit",
  "window_seconds",
];

const show = (value: number | null | undefined) =>
  value === null || value === undefined ? "" : String(value);

const initialValues = (
  scope: QuotaScopeObject | null | undefined,
  provider: Provider | undefined
): Values => ({
  messages_per_day: show(scope?.messages_per_day),
  provider: scope?.provider ?? provider ?? "ses",
  recipients_per_day: show(scope?.recipients_per_day),
  scope_key: scope?.scope_key ?? "",
  window_limit: show(scope?.window_limit),
  window_seconds: show(scope?.window_seconds),
  window_unit: scope?.window_unit ?? "",
});

/** Saves the form: a new scope, or the limits of one (an empty limit clears it). */
const saveScope = async (
  workspace: Workspace,
  scope: QuotaScopeObject | null | undefined,
  values: Values
): Promise<QuotaScopeObject> => {
  const unit = (values.window_unit || null) as WindowUnit | null;
  if (scope) {
    return await workspace.api.quotaScopes.update(scope.id, {
      messages_per_day: integer(values, "messages_per_day") ?? null,
      recipients_per_day: integer(values, "recipients_per_day") ?? null,
      window_limit: integer(values, "window_limit") ?? null,
      window_seconds: integer(values, "window_seconds") ?? null,
      window_unit: unit,
    });
  }
  return await workspace.api.quotaScopes.create({
    messages_per_day: integer(values, "messages_per_day"),
    provider: values.provider as Provider,
    recipients_per_day: integer(values, "recipients_per_day"),
    scope_key: values.scope_key?.trim() ?? "",
    window_limit: integer(values, "window_limit"),
    window_seconds: integer(values, "window_seconds"),
    window_unit: unit ?? undefined,
  });
};

/** A provider's pause of a scope: until when, and what the provider said. */
const PausedNote = ({ scope }: { scope: QuotaScopeObject }) => {
  const paused = holdUntil(scope.paused_until, useNow());
  return paused ? (
    <Alert variant="warning">
      <HugeiconsIcon icon={Alert02Icon} />
      <AlertTitle>Paused until {formatTimestamp(paused)}</AlertTitle>
      <AlertDescription>
        {scope.paused_detail ??
          `${providerLabel(scope.provider)} named this account's shared limit.`}{" "}
        Its connections wait; the pause lifts on its own.
      </AlertDescription>
    </Alert>
  ) : null;
};

/** The fields of a scope: which account (when new), its daily limits and its short window. */
const ScopeFields = ({
  fixedProvider,
  problems,
  scope,
  set,
  values,
}: {
  fixedProvider: boolean;
  problems: Readonly<Record<string, string>>;
  scope: QuotaScopeObject | null | undefined;
  set: (name: string, value: string) => void;
  values: Values;
}) => {
  const info = providerInfo(values.provider ?? "");
  return (
    <>
      {scope || fixedProvider ? null : (
        <SelectRow
          label="Provider"
          name="provider"
          options={PROVIDER_OPTIONS}
          problems={problems}
          set={set}
          values={values}
        />
      )}
      {scope ? null : (
        <TextRow
          autoComplete="off"
          description={info?.scopeKey}
          label="Account"
          mono
          name="scope_key"
          problems={problems}
          required
          set={set}
          values={values}
        />
      )}
      <div className="grid gap-4 sm:grid-cols-2">
        <TextRow
          description="Across its connections, over a rolling day."
          inputMode="numeric"
          label="Messages a day"
          min={1}
          name="messages_per_day"
          optional
          problems={problems}
          set={set}
          type="number"
          values={values}
        />
        <TextRow
          description="Each recipient of a message counts."
          inputMode="numeric"
          label="Recipients a day"
          min={1}
          name="recipients_per_day"
          optional
          problems={problems}
          set={set}
          type="number"
          values={values}
        />
      </div>
      <div className="flex flex-col gap-2">
        <div className="grid gap-4 sm:grid-cols-3">
          <TextRow
            inputMode="numeric"
            label="Short-window limit"
            min={1}
            name="window_limit"
            optional
            problems={problems}
            set={set}
            type="number"
            values={values}
          />
          <SelectRow
            label="Counted in"
            name="window_unit"
            optional
            options={UNIT_OPTIONS}
            placeholder="Unit"
            problems={problems}
            set={set}
            values={values}
          />
          <TextRow
            inputMode="numeric"
            label="Per seconds"
            max={3600}
            min={1}
            name="window_seconds"
            optional
            problems={problems}
            set={set}
            type="number"
            values={values}
          />
        </div>
        <p className="text-fg-3 text-xs">
          The provider&apos;s rate, such as SES&apos;s maximum send rate: 14
          recipients per 1 second. Give all three, or none.
        </p>
      </div>
    </>
  );
};

/** The form inside the dialog, mounted while it is open so it starts from the scope each time. */
const ScopeForm = ({
  onDone,
  onSaved,
  provider,
  scope,
}: {
  onDone: () => void;
  onSaved?: (scope: QuotaScopeObject) => void;
  provider?: Provider;
  scope?: QuotaScopeObject | null;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const { set, values } = useValues(() => initialValues(scope, provider));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const ready = Boolean(scope) || Boolean(values.scope_key?.trim());

  const submit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    // React carries a submit through portals: a connection's form that opened this dialog must
    // not submit with it.
    event.stopPropagation();
    setBusy(true);
    setFailure(null);
    try {
      const saved = await saveScope(workspace, scope, values);
      await queryClient.invalidateQueries({
        queryKey: quotaScopesKey(workspace),
      });
      toast.success(scope ? "Quota scope saved" : "Quota scope created");
      onSaved?.(saved);
      onDone();
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <form className="flex min-h-0 flex-col" onSubmit={submit}>
      <DialogHeader>
        <DialogTitle>
          {scope ? "Edit quota scope" : "New quota scope"}
        </DialogTitle>
        <DialogDescription>
          {scope
            ? `${providerLabel(scope.provider)} · ${scope.scope_key}. An empty limit is cleared.`
            : "The limits of a provider account you own, shared by every connection that names it. The sender stays under each of them."}
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        {scope ? <PausedNote scope={scope} /> : null}
        <ScopeFields
          fixedProvider={Boolean(provider)}
          problems={problems}
          scope={scope}
          set={set}
          values={values}
        />
        <SaveFailure
          failure={failure}
          unplaced={unplacedProblems(problems, FIELDS)}
        />
      </DialogBody>
      <DialogActions>
        <SubmitButton busy={busy} disabled={!ready}>
          {scope ? "Save limits" : "Create scope"}
        </SubmitButton>
      </DialogActions>
    </form>
  );
};

/**
 * Creates a quota scope, or edits one's limits: the daily messages and recipients and the short
 * window a provider account allows, shared by the connections that name it. A connection's form
 * opens it with its provider fixed and picks the new scope when it is saved.
 */
export const QuotaScopeDialog = ({
  onOpenChange,
  onSaved,
  open,
  provider,
  scope,
}: {
  onOpenChange: (open: boolean) => void;
  onSaved?: (scope: QuotaScopeObject) => void;
  open: boolean;
  /** The provider of a new scope, fixed; without it the form asks. */
  provider?: Provider;
  /** The scope to edit; without it the dialog creates one. */
  scope?: QuotaScopeObject | null;
}) => (
  <Dialog onOpenChange={onOpenChange} open={open}>
    <DialogContent className="max-w-[600px]">
      {open ? (
        <ScopeForm
          onDone={() => onOpenChange(false)}
          onSaved={onSaved}
          provider={provider}
          scope={scope}
        />
      ) : null}
    </DialogContent>
  </Dialog>
);
