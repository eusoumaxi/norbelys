import { useState } from "react";
import type { ReactNode } from "react";

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
import { Textarea } from "@/components/ui/textarea";
import { FormField } from "@/lib/form";
import { fieldProblems, problemLine } from "@/lib/problem";

/** One field of a `CreateDialog`, named as the API names it. */
interface DialogField {
  name: string;
  label: string;
  placeholder?: string;
  description?: ReactNode;
  type?: "text" | "email" | "url" | "number" | "datetime-local" | "textarea";
  required?: boolean;
  mono?: boolean;
  /** A choice instead of free text. */
  options?: { label: string; value: string }[];
  initial?: string;
}

/** One field's control: a choice, a text area, or an input of the field's type. */
const Control = ({
  autoFocus,
  field,
  id,
  invalid,
  onChange,
  value,
}: {
  autoFocus: boolean;
  field: DialogField;
  id: string;
  invalid: boolean;
  onChange: (value: string) => void;
  value: string;
}) => {
  if (field.options) {
    return (
      <Select
        id={id}
        onChange={onChange}
        options={field.options}
        value={value}
      />
    );
  }
  // What a text area and an input share; an input also has its type.
  const text = {
    "aria-invalid": invalid,
    autoFocus,
    className: field.mono ? "font-mono" : undefined,
    id,
    onChange: (event: { target: { value: string } }) =>
      onChange(event.target.value),
    placeholder: field.placeholder,
    required: field.required,
    value,
  };
  if (field.type === "textarea") {
    return <Textarea {...text} />;
  }
  return <Input {...text} type={field.type ?? "text"} />;
};

/**
 * A dialog that creates one thing from a few fields: the API's own validation shows beside each
 * field (its paths are the fields' names), anything else under them. `onSubmit` gets the trimmed
 * values; empty optional fields are left out.
 */
export const CreateDialog = ({
  description,
  fields,
  onOpenChange,
  onSubmit,
  open,
  submitLabel,
  title,
  children,
}: {
  description?: ReactNode;
  fields: DialogField[];
  onOpenChange: (open: boolean) => void;
  onSubmit: (values: Record<string, string>) => Promise<unknown>;
  open: boolean;
  submitLabel: string;
  title: string;
  /** Shown under the fields, such as a secret revealed once. */
  children?: ReactNode;
}) => {
  const initial = () =>
    Object.fromEntries(
      fields.map((f) => [f.name, f.initial ?? f.options?.[0]?.value ?? ""])
    );
  const [values, setValues] = useState<Record<string, string>>(initial);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const problems = fieldProblems(failure);
  const unplaced = failure && !fields.some((f) => problems[f.name]);
  const ready = fields.every((f) => !f.required || values[f.name]?.trim());

  return (
    <Dialog
      onOpenChange={(next) => {
        onOpenChange(next);
        if (!next) {
          setFailure(null);
          setValues(initial());
        }
      }}
      open={open}
    >
      <DialogContent>
        <form
          className="flex min-h-0 flex-col"
          onSubmit={async (event) => {
            event.preventDefault();
            setBusy(true);
            setFailure(null);
            const trimmed = Object.fromEntries(
              Object.entries(values)
                .map(([key, value]) => [key, value.trim()] as const)
                .filter(([, value]) => value !== "")
            );
            try {
              await onSubmit(trimmed);
              setValues(initial());
            } catch (error) {
              setFailure(error);
            }
            setBusy(false);
          }}
        >
          <DialogHeader>
            <DialogTitle>{title}</DialogTitle>
            {description ? (
              <DialogDescription>{description}</DialogDescription>
            ) : null}
          </DialogHeader>
          <DialogBody>
            {fields.map((field, index) => {
              const id = `field-${field.name}`;
              const problem = problems[field.name];
              return (
                <FormField
                  description={field.description}
                  htmlFor={id}
                  key={field.name}
                  label={field.label}
                  optional={!field.required}
                  problem={problem}
                >
                  <Control
                    autoFocus={index === 0}
                    field={field}
                    id={id}
                    invalid={Boolean(problem)}
                    onChange={(value) =>
                      setValues((v) => ({ ...v, [field.name]: value }))
                    }
                    value={values[field.name] ?? ""}
                  />
                </FormField>
              );
            })}
            {children}
            {unplaced ? (
              <ProblemAlert>{problemLine(failure)}</ProblemAlert>
            ) : null}
          </DialogBody>
          <DialogActions>
            <SubmitButton busy={busy} disabled={!ready}>
              {submitLabel}
            </SubmitButton>
          </DialogActions>
        </form>
      </DialogContent>
    </Dialog>
  );
};
