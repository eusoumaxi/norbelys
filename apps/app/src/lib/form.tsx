import { createFormHook, createFormHookContexts } from "@tanstack/react-form";
import type { ComponentProps, ReactNode } from "react";

import { SubmitButton } from "@/components/dialog-actions";
import { SaveFailure } from "@/components/problem";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
  FieldRequirement,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { fieldProblems, unplacedProblems } from "@/lib/problem";

/**
 * One labelled control of a form: the label (with "optional" when the field may stay empty), the
 * control, then one line under it: the problem with the field while there is one, else its
 * description.
 */
export const FormField = ({
  children,
  className,
  description,
  htmlFor,
  label,
  optional = false,
  problem,
}: {
  children: ReactNode;
  className?: string;
  description?: ReactNode;
  htmlFor?: string;
  label: ReactNode;
  optional?: boolean;
  problem?: string | null;
}) => (
  <Field className={className}>
    <FieldLabel htmlFor={htmlFor}>
      {label}
      {optional ? <FieldRequirement requirement="optional" /> : null}
    </FieldLabel>
    {children}
    {problem ? <FieldError>{problem}</FieldError> : null}
    {!problem && description ? (
      <FieldDescription>{description}</FieldDescription>
    ) : null}
  </Field>
);

/** A switch with its label beside it and, when given, a line of explanation under it. */
export const SwitchField = ({
  checked,
  description,
  id,
  label,
  onChange,
}: {
  checked: boolean;
  description?: ReactNode;
  id: string;
  label: ReactNode;
  onChange: (checked: boolean) => void;
}) => (
  <div className="flex flex-col gap-1">
    <Label className="text-fg gap-3 font-normal" htmlFor={id}>
      <Switch checked={checked} id={id} onCheckedChange={onChange} />
      {label}
    </Label>
    {description ? (
      <p className="text-fg-3 pl-10 text-xs">{description}</p>
    ) : null}
  </div>
);

/**
 * The browser's own time zone: a new time zone field's default, and the zone a time typed in the
 * browser is read in. UTC when the browser keeps it to itself.
 */
export const BROWSER_ZONE = (() => {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return "UTC";
  }
})();

/** Every IANA zone the browser knows, the choices of a time zone field; UTC alone if it lists none. */
export const TIME_ZONES: readonly string[] = (() => {
  try {
    return Intl.supportedValuesOf("timeZone");
  } catch {
    return ["UTC"];
  }
})();

/** The time zones offered while one is typed, for an input whose `list` is `id`. */
export const TimeZoneOptions = ({ id }: { id: string }) => (
  <datalist id={id}>
    {TIME_ZONES.map((zone) => (
      <option key={zone} value={zone}>
        {zone}
      </option>
    ))}
  </datalist>
);

const { fieldContext, formContext, useFieldContext, useFormContext } =
  createFormHookContexts();

/** zod reports Standard Schema issues; the API reports plain strings (see `toFormErrors`). */
const messageOf = (error: unknown): string | undefined => {
  if (typeof error === "string") {
    return error;
  }
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return error.message;
  }
};

interface FrameProps {
  children: (control: {
    "aria-invalid": boolean;
    id: string;
    name: string;
    onBlur: () => void;
    value: string;
  }) => ReactNode;
  description?: string;
  label: string;
  requirement?: "optional" | "required";
}

/** A TanStack field's frame: the first error while it is invalid and touched, as `FormField` shows it. */
const FieldFrame = ({
  children,
  description,
  label,
  requirement,
}: FrameProps) => {
  const field = useFieldContext<string>();
  const invalid = field.state.meta.isTouched && !field.state.meta.isValid;
  const error = invalid
    ? field.state.meta.errors.map(messageOf).find(Boolean)
    : undefined;
  return (
    <FormField
      description={description}
      htmlFor={field.name}
      label={label}
      optional={requirement === "optional"}
      problem={error}
    >
      {children({
        "aria-invalid": invalid,
        id: field.name,
        name: field.name,
        onBlur: field.handleBlur,
        value: field.state.value,
      })}
    </FormField>
  );
};

type ControlProps<C extends "input" | "textarea"> = Omit<
  ComponentProps<C>,
  "id" | "name" | "onBlur" | "onChange" | "value"
> &
  Pick<FrameProps, "description" | "label" | "requirement">;

const TextField = ({
  description,
  label,
  requirement,
  ...props
}: ControlProps<"input">) => {
  const field = useFieldContext<string>();
  return (
    <FieldFrame
      description={description}
      label={label}
      requirement={requirement}
    >
      {(control) => (
        <Input
          {...props}
          {...control}
          onChange={(event) => field.handleChange(event.target.value)}
        />
      )}
    </FieldFrame>
  );
};

const TextareaField = ({
  description,
  label,
  requirement,
  ...props
}: ControlProps<"textarea">) => {
  const field = useFieldContext<string>();
  return (
    <FieldFrame
      description={description}
      label={label}
      requirement={requirement}
    >
      {(control) => (
        <Textarea
          {...props}
          {...control}
          onChange={(event) => field.handleChange(event.target.value)}
        />
      )}
    </FieldFrame>
  );
};

/**
 * The form element of a dialog: `contents`, so the dialog's body and footer stay its bands;
 * submitting runs the form's validation, then its submission.
 */
const DialogForm = ({ children }: { children: ReactNode }) => {
  const form = useFormContext();
  return (
    <form
      className="contents"
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        void form.handleSubmit();
      }}
    >
      {children}
    </form>
  );
};

/** The form's primary action, in the dialog footer, with a spinner while the form submits. */
const FormSubmitButton = ({ children }: { children: ReactNode }) => {
  const form = useFormContext();
  return (
    <form.Subscribe selector={(state) => state.isSubmitting}>
      {(isSubmitting) => (
        <SubmitButton busy={isSubmitting}>{children}</SubmitButton>
      )}
    </form.Subscribe>
  );
};

/** Why the last submission was refused, with the API's problems no field of the form shows. */
const submissionError = (state: {
  errorMap: { onSubmit?: unknown };
}): unknown => state.errorMap.onSubmit;

const FormError = () => {
  const form = useFormContext();
  const names = Object.keys(form.state.values);
  return (
    <form.Subscribe selector={submissionError}>
      {(failure) => (
        <SaveFailure
          failure={failure}
          unplaced={unplacedProblems(fieldProblems(failure), names)}
        />
      )}
    </form.Subscribe>
  );
};

export const { useAppForm } = createFormHook({
  fieldComponents: { TextField, TextareaField },
  fieldContext,
  formComponents: { DialogForm, FormError, SubmitButton: FormSubmitButton },
  formContext,
});

/**
 * Turns a failed save into TanStack Form's `{ form, fields }`: the API's field problems show next
 * to their inputs (its paths are the form's names), and the failure itself under the form. Return
 * it from `validators.onSubmitAsync`.
 */
export const toFormErrors = (error: unknown) => ({
  fields: fieldProblems(error),
  form: error,
});
