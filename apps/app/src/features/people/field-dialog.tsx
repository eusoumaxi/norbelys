import type { FieldObject, FieldType } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import type { SubmitEvent } from "react";
import { toast } from "sonner";

import { DialogActions, SubmitButton } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import {
  FIELD_TYPES,
  fieldTypeLabel,
  keyFromLabel,
} from "@/features/people/fields";
import { fieldsKey } from "@/features/people/queries";
import { ValuesInput } from "@/features/people/values-input";
import { FormField } from "@/lib/form";
import { fieldProblems, problemAt, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";
import type { Workspace } from "@/lib/workspace";

const TYPE_OPTIONS = FIELD_TYPES.map((entry) => ({
  label: entry.label,
  value: entry.value,
}));

/** The key and type of an existing field, which never change. */
const FixedIdentity = ({ field }: { field: FieldObject }) => (
  <div className="border-line bg-chrome flex flex-wrap items-center gap-x-6 gap-y-1 rounded-sm border px-3 py-2 text-sm">
    <span className="flex items-center gap-2">
      <span className="text-fg-3">Key</span>
      <code className="text-fg font-mono text-xs">{field.key}</code>
    </span>
    <span className="flex items-center gap-2">
      <span className="text-fg-3">Type</span>
      <span className="text-fg">{fieldTypeLabel(field.type)}</span>
    </span>
    <span className="text-fg-3 text-xs">Neither can change.</span>
  </div>
);

/** The key and the type of a new field, which never change afterwards. */
const KeyAndType = ({
  fieldKey,
  onKeyChange,
  onTypeChange,
  problems,
  type,
}: {
  fieldKey: string;
  onKeyChange: (key: string) => void;
  onTypeChange: (type: FieldType) => void;
  problems: Readonly<Record<string, string>>;
  type: FieldType;
}) => {
  const typeDescription = FIELD_TYPES.find(
    (entry) => entry.value === type
  )?.description;
  return (
    <>
      <FormField
        description="How the API, CSV columns and segment conditions name it (fields.<key>). A lowercase letter, then lowercase letters, digits or underscores. It can't change later."
        htmlFor="field-key"
        label="Key"
        problem={problems.key}
      >
        <Input
          aria-invalid={Boolean(problems.key)}
          autoComplete="off"
          className="font-mono"
          id="field-key"
          maxLength={64}
          onChange={(event) => onKeyChange(event.target.value)}
          placeholder="industry"
          spellCheck={false}
          value={fieldKey}
        />
      </FormField>
      <FormField
        description={`${typeDescription ?? ""} The type can't change later.`}
        htmlFor="field-type"
        label="Type"
        problem={problems.type}
      >
        <Select
          id="field-type"
          onChange={(value) => onTypeChange(value as FieldType)}
          options={TYPE_OPTIONS}
          value={type}
        />
      </FormField>
    </>
  );
};

/** An enum's options, with what an edit may not remove. */
const OptionsField = ({
  editing,
  onChange,
  options,
  problem,
}: {
  editing: boolean;
  onChange: (options: string[]) => void;
  options: string[];
  problem?: string;
}) => (
  <FormField
    description={
      editing
        ? "An option some person still holds can't be removed, and neither can one a segment filters on."
        : "1 to 100 distinct options. Press Enter after each one."
    }
    htmlFor="field-options"
    label="Options"
    problem={problem}
  >
    <ValuesInput
      id="field-options"
      invalid={Boolean(problem)}
      onChange={onChange}
      placeholder="Type an option, then press Enter"
      values={options}
    />
  </FormField>
);

interface Draft {
  key: string;
  label: string;
  options: string[];
  type: FieldType;
}

/** Sends the draft: `fields.update` for an existing field, `fields.create` otherwise. */
const saveField = async (
  workspace: Workspace,
  field: FieldObject | undefined,
  draft: Draft
) => {
  const isEnum = (field?.type ?? draft.type) === "enum";
  const options = isEnum ? { options: draft.options } : {};
  if (field) {
    await workspace.api.fields.update(
      field.id,
      { label: draft.label.trim(), ...options },
      { headers: { "If-Match": `"${field.version}"` } }
    );
    return;
  }
  await workspace.api.fields.create({
    key: draft.key.trim(),
    label: draft.label.trim(),
    type: draft.type,
    ...options,
  });
};

/** Whether a control shows the API's message, so no summary is needed. */
const isPlaced = (problems: Readonly<Record<string, string>>): boolean =>
  Boolean(
    problems.key ||
    problems.label ||
    problems.type ||
    problemAt(problems, "options")
  );

/** The label control. */
const LabelField = ({
  onChange,
  problem,
  value,
}: {
  onChange: (label: string) => void;
  problem?: string;
  value: string;
}) => (
  <FormField
    description="The name people read, in forms and lists."
    htmlFor="field-label"
    label="Label"
    problem={problem}
  >
    <Input
      aria-invalid={Boolean(problem)}
      autoFocus
      id="field-label"
      maxLength={100}
      onChange={(event) => onChange(event.target.value)}
      placeholder="Industry"
      value={value}
    />
  </FormField>
);

interface FormProps {
  field?: FieldObject;
  onSaved: () => void;
}

/** Create a field (key, label, type, an enum's options) or edit one (label, options). */
const FieldForm = ({ field, onSaved }: FormProps) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<Draft>({
    key: field?.key ?? "",
    label: field?.label ?? "",
    options: field?.options ?? [],
    type: "text",
  });
  // Until the key is typed by hand, it follows the label.
  const [keyTyped, setKeyTyped] = useState(Boolean(field));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const optionProblem = problemAt(problems, "options");
  const placed = isPlaced(problems);
  const change = (next: Partial<Draft>) =>
    setDraft((current) => ({ ...current, ...next }));
  const ready = Boolean(draft.label.trim() && draft.key.trim());

  const submit = async (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await saveField(workspace, field, draft);
      toast.success(field ? "Field saved" : "Field created");
      void queryClient.invalidateQueries({ queryKey: fieldsKey(workspace) });
      onSaved();
    } catch (error) {
      setFailure(error);
    }
    setBusy(false);
  };

  return (
    <form
      className="contents"
      noValidate
      onSubmit={(event) => {
        void submit(event);
      }}
    >
      <DialogBody>
        {field ? <FixedIdentity field={field} /> : null}
        <LabelField
          onChange={(label) =>
            change({
              label,
              ...(keyTyped ? {} : { key: keyFromLabel(label) }),
            })
          }
          problem={problems.label}
          value={draft.label}
        />
        {field ? null : (
          <KeyAndType
            fieldKey={draft.key}
            onKeyChange={(key) => {
              change({ key });
              setKeyTyped(true);
            }}
            onTypeChange={(type) => change({ type })}
            problems={problems}
            type={draft.type}
          />
        )}
        {(field?.type ?? draft.type) === "enum" ? (
          <OptionsField
            editing={Boolean(field)}
            onChange={(options) => change({ options })}
            options={draft.options}
            problem={optionProblem}
          />
        ) : null}
        {failure && !placed ? (
          <ProblemAlert>{problemLine(failure)}</ProblemAlert>
        ) : null}
      </DialogBody>
      <DialogActions>
        <SubmitButton busy={busy} disabled={!ready}>
          {field ? "Save changes" : "Create field"}
        </SubmitButton>
      </DialogActions>
    </form>
  );
};

/**
 * Creates a custom field (`fields.create`: key, label, type, and the options of an enum) or edits
 * one (`fields.update`: its label and an enum's options, with its version in `If-Match`). A key
 * is suggested from the label until it is typed by hand; the API's messages show beside the
 * control they concern.
 */
export const FieldDialog = ({
  field,
  onOpenChange,
  open,
}: {
  /** The field to edit; none to create one. */
  field?: FieldObject | null;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => (
  <Dialog onOpenChange={onOpenChange} open={open}>
    <DialogContent>
      <DialogHeader>
        <DialogTitle>{field ? "Edit field" : "Create field"}</DialogTitle>
        <DialogDescription>
          {field
            ? "A new label shows everywhere at once; stored values keep their meaning."
            : "A typed attribute every person can hold, such as an industry, a plan or a renewal date."}
        </DialogDescription>
      </DialogHeader>
      <FieldForm
        field={field ?? undefined}
        key={field ? `${field.id}:${field.version}` : "new"}
        onSaved={() => onOpenChange(false)}
      />
    </DialogContent>
  </Dialog>
);
