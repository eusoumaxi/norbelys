import type { ImapSecurity, SmtpSecurity as Security } from "@norbelys/sdk";
import { cn } from "cn";
import { useState } from "react";
import type { ComponentProps, ReactNode } from "react";

import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import type { SelectOption } from "@/components/ui/select";
import {
  IMAP_SECURITY_OPTIONS,
  imapPortFor,
  portFor,
  SECURITY_OPTIONS,
} from "@/features/mailboxes/providers";
import { FormField } from "@/lib/form";
import { problemAt } from "@/lib/problem";

/** A form's values by the API's field path (`smtp.host`), so its problems land on their inputs. */
export type Values = Record<string, string>;

/** Sets one value of a form. */
export type SetValue = (name: string, value: string) => void;

/** The values of a form and the setter of one of them. */
export const useValues = (initial: () => Values) => {
  const [values, setValues] = useState<Values>(initial);
  const set: SetValue = (name, value) => {
    setValues((current) => ({ ...current, [name]: value }));
  };
  return { reset: setValues, set, values };
};

/** A value as the API takes it: trimmed, or left out when empty. */
export const optional = (values: Values, name: string): string | undefined =>
  values[name]?.trim() || undefined;

/** A whole number, or left out when empty. */
export const integer = (values: Values, name: string): number | undefined => {
  const text = values[name]?.trim();
  return text ? Number(text) : undefined;
};

/** Addresses or names written one per line or separated by commas. */
export const splitList = (text: string | undefined): string[] =>
  (text ?? "")
    .split(/[\n,]/u)
    .map((item) => item.trim())
    .filter(Boolean);

/** The id of the input bound to a field path. */
export const inputId = (name: string) =>
  `field-${name.replaceAll(/\W/gu, "-")}`;

/** A text input bound to one path of a form's values, with its label and its problem. */
export const TextRow = ({
  className,
  description,
  label,
  mono = false,
  name,
  optional: isOptional = false,
  problems,
  set,
  values,
  ...input
}: Omit<
  ComponentProps<"input">,
  "id" | "name" | "onChange" | "value" | "className"
> & {
  className?: string;
  description?: ReactNode;
  label: ReactNode;
  mono?: boolean;
  name: string;
  optional?: boolean;
  problems: Readonly<Record<string, string>>;
  set: SetValue;
  values: Values;
}) => {
  const problem = problemAt(problems, name);
  return (
    <FormField
      className={className}
      description={description}
      htmlFor={inputId(name)}
      label={label}
      optional={isOptional}
      problem={problem}
    >
      <Input
        {...input}
        aria-invalid={Boolean(problem)}
        className={mono ? "font-mono" : undefined}
        id={inputId(name)}
        onChange={(event) => set(name, event.target.value)}
        value={values[name] ?? ""}
      />
    </FormField>
  );
};

/** A choice bound to one path of a form's values. */
export const SelectRow = ({
  className,
  description,
  label,
  name,
  onChange,
  optional: isOptional = false,
  options,
  placeholder,
  problems,
  set,
  values,
}: {
  className?: string;
  description?: ReactNode;
  label: ReactNode;
  name: string;
  /** Runs instead of setting the value, for a choice that changes other values too. */
  onChange?: (value: string) => void;
  optional?: boolean;
  options: SelectOption[];
  placeholder?: string;
  problems: Readonly<Record<string, string>>;
  set: SetValue;
  values: Values;
}) => (
  <FormField
    className={className}
    description={description}
    htmlFor={inputId(name)}
    label={label}
    optional={isOptional}
    problem={problemAt(problems, name)}
  >
    <Select
      id={inputId(name)}
      onChange={(value) => (onChange ? onChange(value) : set(name, value))}
      options={options}
      placeholder={placeholder}
      value={values[name] || null}
    />
  </FormField>
);

/**
 * The host, port and security of an SMTP or IMAP server, as connecting an account and its
 * settings edit them: choosing a security moves a port still at the old one's default to the new
 * one's. `required` makes the browser refuse an empty host or port.
 */
export const Endpoint = ({
  hostDescription,
  hostPlaceholder,
  prefix,
  problems,
  required = false,
  set,
  values,
}: {
  hostDescription?: string;
  hostPlaceholder?: string;
  prefix: "smtp" | "imap";
  problems: Readonly<Record<string, string>>;
  required?: boolean;
  set: SetValue;
  values: Values;
}) => (
  <>
    <div className="grid gap-4 sm:grid-cols-[1fr_120px]">
      <TextRow
        autoComplete="off"
        description={hostDescription}
        label="Host"
        mono
        name={`${prefix}.host`}
        placeholder={hostPlaceholder}
        problems={problems}
        required={required}
        set={set}
        values={values}
      />
      <TextRow
        inputMode="numeric"
        label="Port"
        max={65_535}
        min={1}
        name={`${prefix}.port`}
        problems={problems}
        required={required}
        set={set}
        type="number"
        values={values}
      />
    </div>
    <SelectRow
      label="Security"
      name={`${prefix}.security`}
      onChange={(next) => {
        const port = values[`${prefix}.port`] ?? "";
        set(
          `${prefix}.port`,
          prefix === "smtp"
            ? portFor(
                (values["smtp.security"] ?? "starttls") as Security,
                next as Security,
                port
              )
            : imapPortFor(
                (values["imap.security"] ?? "tls") as ImapSecurity,
                next as ImapSecurity,
                port
              )
        );
        set(`${prefix}.security`, next);
      }}
      options={prefix === "smtp" ? SECURITY_OPTIONS : IMAP_SECURITY_OPTIONS}
      problems={problems}
      set={set}
      values={values}
    />
  </>
);

/** A titled group of fields inside a form: a 14px heading, a line of explanation, the fields. */
export const FormSection = ({
  children,
  className,
  description,
  title,
}: {
  children: ReactNode;
  className?: string;
  description?: ReactNode;
  title: ReactNode;
}) => (
  <section className={cn("flex min-w-0 flex-col gap-4", className)}>
    <div className="flex flex-col gap-1">
      <h3 className="text-fg text-base font-medium">{title}</h3>
      {description ? <p className="text-fg-2 text-xs">{description}</p> : null}
    </div>
    {children}
  </section>
);
