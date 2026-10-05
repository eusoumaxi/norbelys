import { Add01Icon, Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { Combine, Operator } from "@norbelys/sdk";
import { cn } from "cn";

import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { ValuesInput } from "@/features/people/values-input";
import type { DraftCondition, Kind, Subject } from "@/features/segments/filter";
import {
  blankCondition,
  OPERATORS,
  operatorLabel,
  subjectOf,
  takesValue,
} from "@/features/segments/filter";

/** A filter holds at most this many conditions. */
const CONDITIONS_MAX = 20;

const MATCH_OPTIONS: { label: string; value: Combine }[] = [
  { label: "All conditions", value: "all" },
  { label: "Any condition", value: "any" },
];

const BOOLEAN_OPTIONS = [
  { label: "Yes", value: "true" },
  { label: "No", value: "false" },
];

const PLACEHOLDERS: Partial<Record<Kind, string>> = {
  address: "ada@example.com",
  domain: "example.com",
  number: "0",
  text: "Value",
};

interface ValueProps {
  condition: DraftCondition;
  invalid: boolean;
  label: string;
  onChange: (change: Partial<DraftCondition>) => void;
  subject: Subject;
}

/** `in` over a closed set (an enum's options, yes and no): toggles, one per choice. */
const ChoiceToggles = ({
  choices,
  condition,
  label,
  onChange,
}: {
  choices: { label: string; value: string }[];
  condition: DraftCondition;
  label: string;
  onChange: (change: Partial<DraftCondition>) => void;
}) => (
  <fieldset aria-label={label} className="flex min-w-0 flex-wrap gap-1.5">
    {choices.map((choice) => {
      const on = condition.values.includes(choice.value);
      return (
        <button
          aria-pressed={on}
          className={cn(
            "border-line text-fg focus-visible:outline-focus h-8 cursor-pointer rounded-sm border px-3 text-sm transition-colors outline-none focus-visible:outline-1",
            on
              ? "bg-selected border-line-strong font-semibold"
              : "hover:bg-hover"
          )}
          key={choice.value}
          onClick={() =>
            onChange({
              values: on
                ? condition.values.filter((value) => value !== choice.value)
                : [...condition.values, choice.value],
            })
          }
          type="button"
        >
          {choice.label}
        </button>
      );
    })}
  </fieldset>
);

/** The choices of a closed field: an enum's options, or yes and no. */
const choicesOf = (subject: Subject) =>
  subject.kind === "boolean"
    ? BOOLEAN_OPTIONS
    : subject.options.map((option) => ({ label: option, value: option }));

/** The value control of a condition, by its field's kind and its operator. */
const ValueControl = ({
  condition,
  invalid,
  label,
  onChange,
  subject,
}: ValueProps) => {
  const closed = subject.kind === "choice" || subject.kind === "boolean";
  if (condition.operator === "in") {
    return closed ? (
      <ChoiceToggles
        choices={choicesOf(subject)}
        condition={condition}
        label={label}
        onChange={onChange}
      />
    ) : (
      <ValuesInput
        invalid={invalid}
        label={label}
        onChange={(values) => onChange({ values })}
        values={condition.values}
      />
    );
  }
  if (closed) {
    return (
      <Select
        label={label}
        onChange={(value) => onChange({ value })}
        options={choicesOf(subject)}
        value={condition.value || null}
      />
    );
  }
  let type = "text";
  if (subject.kind === "instant") {
    type = "datetime-local";
  } else if (subject.kind === "date") {
    type = "date";
  } else if (subject.kind === "number") {
    type = "number";
  }
  return (
    <Input
      aria-invalid={invalid}
      aria-label={label}
      onChange={(event) => onChange({ value: event.target.value })}
      placeholder={PLACEHOLDERS[subject.kind]}
      step={subject.kind === "number" ? "any" : undefined}
      type={type}
      value={condition.value}
    />
  );
};

interface RowProps {
  condition: DraftCondition;
  index: number;
  lead: string;
  onChange: (next: DraftCondition) => void;
  onRemove: (() => void) | null;
  problems: Readonly<Record<string, string>>;
  subjects: Subject[];
}

/** One condition: where/and/or, the field, the operator, the value, and a remove button. */
const ConditionRow = ({
  condition,
  index,
  lead,
  onChange,
  onRemove,
  problems,
  subjects,
}: RowProps) => {
  const subject = subjectOf(subjects, condition.field);
  const path = `filter.conditions[${index}]`;
  const problem =
    problems[`${path}.field`] ??
    problems[`${path}.operator`] ??
    problems[`${path}.value`] ??
    problems[path];
  const fieldOptions = subjects.map((option) => ({
    label: option.custom ? `${option.label} (custom)` : option.label,
    value: option.field,
  }));
  const operatorOptions = OPERATORS[subject.kind].map((operator) => ({
    label: operatorLabel(subject.kind, operator),
    value: operator,
  }));
  return (
    <li className="flex flex-col gap-1.5">
      <div className="grid grid-cols-[48px_minmax(0,1fr)_32px] items-start gap-2 sm:grid-cols-[48px_minmax(0,1fr)_minmax(0,0.9fr)_minmax(0,1.3fr)_32px]">
        <span className="text-fg-3 h-8 text-right text-sm leading-8">
          {lead}
        </span>
        <Select
          label={`Condition ${index + 1} field`}
          onChange={(field) =>
            onChange({
              ...blankCondition(subjectOf(subjects, field)),
              key: condition.key,
            })
          }
          options={fieldOptions}
          value={condition.field}
        />
        <div className="col-start-2 row-start-2 sm:col-start-auto sm:row-start-auto">
          <Select
            label={`Condition ${index + 1} operator`}
            onChange={(operator) =>
              onChange({ ...condition, operator: operator as Operator })
            }
            options={operatorOptions}
            value={condition.operator}
          />
        </div>
        <div className="col-start-2 row-start-3 min-w-0 sm:col-start-auto sm:row-start-auto">
          {takesValue(condition.operator) ? (
            <ValueControl
              condition={condition}
              invalid={Boolean(problem)}
              label={`Condition ${index + 1} value`}
              onChange={(change) => onChange({ ...condition, ...change })}
              subject={subject}
            />
          ) : (
            <span className="text-fg-3 block h-8 text-sm leading-8">
              No value needed
            </span>
          )}
        </div>
        <Button
          aria-label={`Remove condition ${index + 1}`}
          className="col-start-3 row-start-1 sm:col-start-auto sm:row-start-auto"
          disabled={!onRemove}
          onClick={() => onRemove?.()}
          size="icon-m"
          variant="tertiary"
        >
          <HugeiconsIcon icon={Delete02Icon} />
        </Button>
      </div>
      {problem ? <FieldError className="pl-14">{problem}</FieldError> : null}
    </li>
  );
};

/**
 * The segment filter builder: whether all or any conditions must hold, then up to 20 conditions,
 * each a field (a person's attribute or a custom field), an operator the API accepts for that
 * field's type, and a value in the control its type calls for: none for "is set" and "is not
 * set", a list for "is one of" (toggles for an enum's options). The API's message about a
 * condition shows under it (`filter.conditions[i]…`).
 */
export const FilterBuilder = ({
  conditions,
  match,
  onConditionsChange,
  onMatchChange,
  problems,
  subjects,
}: {
  conditions: DraftCondition[];
  match: Combine;
  onConditionsChange: (conditions: DraftCondition[]) => void;
  onMatchChange: (match: Combine) => void;
  problems: Readonly<Record<string, string>>;
  subjects: Subject[];
}) => {
  const joiner = match === "any" ? "or" : "and";
  const [first] = subjects;
  const general = problems["filter.conditions"] ?? problems.filter;
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-3">
        <span className="text-fg-2 text-sm font-semibold">People matching</span>
        <Segmented
          label="How conditions combine"
          onChange={onMatchChange}
          options={MATCH_OPTIONS}
          value={match}
        />
      </div>
      <ol className="flex flex-col gap-3">
        {conditions.map((condition, index) => (
          <ConditionRow
            condition={condition}
            index={index}
            key={condition.key}
            lead={index === 0 ? "Where" : joiner}
            onChange={(next) =>
              onConditionsChange(
                conditions.map((item) =>
                  item.key === condition.key ? next : item
                )
              )
            }
            onRemove={
              conditions.length > 1
                ? () =>
                    onConditionsChange(
                      conditions.filter((item) => item.key !== condition.key)
                    )
                : null
            }
            problems={problems}
            subjects={subjects}
          />
        ))}
      </ol>
      {general ? <FieldError>{general}</FieldError> : null}
      <div className="pl-14">
        <Button
          disabled={!first || conditions.length >= CONDITIONS_MAX}
          onClick={() => {
            if (first) {
              onConditionsChange([...conditions, blankCondition(first)]);
            }
          }}
          size="s"
          variant="tertiary"
        >
          <HugeiconsIcon icon={Add01Icon} />
          Add condition
        </Button>
      </div>
    </div>
  );
};
