import type { Condition, Filter } from "@norbelys/sdk";

import type { Subject } from "@/features/segments/filter";
import {
  operandsOf,
  operatorLabel,
  subjectOf,
} from "@/features/segments/filter";

/** One condition in words: the field's label, the operator, its values as mono chips. */
const ConditionWords = ({
  condition,
  subjects,
}: {
  condition: Condition;
  subjects: Subject[];
}) => {
  const subject = subjectOf(subjects, condition.field);
  const operands = [...new Set(operandsOf(condition, subject.kind))];
  return (
    <span className="inline-flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-1">
      <span className="text-fg font-semibold">{subject.label}</span>
      <span className="text-fg-2">
        {operatorLabel(subject.kind, condition.operator)}
      </span>
      {operands.map((operand) => (
        <code
          className="border-line bg-chrome text-fg max-w-60 truncate rounded-sm border px-1.5 font-mono text-xs leading-[18px]"
          key={operand}
          title={operand}
        >
          {operand}
        </code>
      ))}
    </span>
  );
};

/**
 * A filter in words, one condition per line: "Where Email domain is gmail.com", then "and" or
 * "or" before each next one, as the filter's `match` combines them.
 */
export const FilterWords = ({
  filter,
  subjects,
}: {
  filter: Filter;
  subjects: Subject[];
}) => {
  const joiner = filter.match === "any" ? "or" : "and";
  const lines = filter.conditions.map((condition, position) => ({
    condition,
    id: `${position}:${JSON.stringify(condition)}`,
    lead: position === 0 ? "Where" : joiner,
  }));
  return (
    <ol className="flex flex-col gap-2.5">
      {lines.map((line) => (
        <li className="flex items-start gap-3 text-sm" key={line.id}>
          <span className="text-fg-3 w-12 shrink-0 text-right leading-5">
            {line.lead}
          </span>
          <ConditionWords condition={line.condition} subjects={subjects} />
        </li>
      ))}
    </ol>
  );
};

/** A filter in one line, for a list: its first condition, then how many more and how. */
export const FilterSummary = ({
  filter,
  subjects,
}: {
  filter: Filter;
  subjects: Subject[];
}) => {
  const [first] = filter.conditions;
  if (!first) {
    return null;
  }
  const more = filter.conditions.length - 1;
  return (
    <span className="flex min-w-0 items-center gap-2 text-sm">
      <span className="truncate">
        <ConditionWords condition={first} subjects={subjects} />
      </span>
      {more > 0 ? (
        <span className="text-fg-3 shrink-0 text-xs">
          {filter.match === "any" ? "or" : "and"} {more} more
        </span>
      ) : null}
    </span>
  );
};
