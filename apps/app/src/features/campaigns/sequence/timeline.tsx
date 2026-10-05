import {
  Add01Icon,
  ArrowDown01Icon,
  ArrowUp01Icon,
  Clock01Icon,
  Delete02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";
import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { RowMenu } from "@/components/row-menu";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { formatDelay } from "@/features/campaigns/format";
import { hasProblem, STEPS_MAX } from "@/features/campaigns/sequence/draft";
import type { StepDraft } from "@/features/campaigns/sequence/draft";
import {
  ProblemDot,
  StepNumber,
  TemplateText,
} from "@/features/campaigns/sequence/parts";
import { DelayInput } from "@/features/campaigns/sequence/step-settings";
import type { FieldName } from "@/features/messages/merge-tags";
import { plural } from "@/lib/format";

/** Whether the person asked the system for less motion. */
const still = () =>
  window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/** The thin line that joins two moments of the sequence. */
const Line = () => (
  <span aria-hidden className="bg-line mx-auto block h-3 w-px" />
);

/**
 * The wait between two steps, editable in place: a number and its unit. Read-only, it is the
 * wait in words.
 */
const Wait = ({
  error,
  onChange,
  position,
  readOnly,
  step,
}: {
  error?: string;
  onChange: (patch: Partial<StepDraft>) => void;
  position: number;
  readOnly: boolean;
  step: StepDraft;
}) => (
  <div className="flex flex-col items-center">
    <Line />
    <div
      className={cn(
        "text-fg-2 flex h-8 items-center gap-1.5 rounded-sm border px-2 text-xs",
        error ? "border-error-line" : "border-line"
      )}
    >
      <HugeiconsIcon
        className="text-icon size-3.5 shrink-0"
        icon={Clock01Icon}
      />
      {readOnly ? (
        <span>
          {step.delay_seconds > 0
            ? `Wait ${formatDelay(step.delay_seconds).toLowerCase()}`
            : "No wait"}
        </span>
      ) : (
        <>
          <span>Wait</span>
          <DelayInput
            compact
            id={`${step.key}-chip`}
            invalid={Boolean(error)}
            label={`Wait before step ${position}`}
            onChange={onChange}
            step={step}
          />
        </>
      )}
    </div>
    {error ? (
      <p className="text-error-fg mt-1 max-w-full text-center text-xs">
        {error}
      </p>
    ) : null}
    <Line />
  </div>
);

/** What a step's line under its subject says, when it differs from one plain email. */
const traits = (step: StepDraft, position: number): string[] =>
  [
    step.variants.length > 1 ? plural(step.variants.length, "variant") : null,
    step.personalised ? "AI" : null,
    position > 1 && !step.same_thread ? "New thread" : null,
    step.winner ? "Winner chosen" : null,
  ].filter((trait) => trait !== null);

/**
 * One step in the timeline: its number, its name, its first subject; a menu moves it, adds a
 * step after it or deletes it. Only the selected step is in the tab order: the arrow keys move
 * through the others.
 */
const StepItem = ({
  fields,
  onAddAfter,
  onDelete,
  onKeyDown,
  onMove,
  onSelect,
  position,
  problem,
  readOnly,
  selected,
  step,
  total,
}: {
  fields: readonly FieldName[];
  onAddAfter: () => void;
  onDelete: () => void;
  onMove: (to: number) => void;
  /** The arrow keys on the step, which move through the steps. */
  onKeyDown: (event: KeyboardEvent<HTMLButtonElement>) => void;
  onSelect: () => void;
  position: number;
  problem: boolean;
  readOnly: boolean;
  selected: boolean;
  step: StepDraft;
  total: number;
}) => {
  const subject = step.variants[0]?.subject.trim() ?? "";
  const meta = traits(step, position);
  return (
    <div
      className={cn(
        "relative rounded-lg border transition-colors duration-120",
        selected ? "border-accent" : "border-line hover:border-line-strong"
      )}
    >
      <button
        aria-current={selected ? "step" : undefined}
        className="focus-visible:outline-focus flex w-full cursor-pointer scroll-mt-12 scroll-mb-16 items-start gap-2.5 rounded-lg p-3 pr-10 text-left outline-none focus-visible:outline-1"
        data-step-index={position - 1}
        onClick={onSelect}
        onKeyDown={onKeyDown}
        tabIndex={selected ? 0 : -1}
        type="button"
      >
        <StepNumber position={position} selected={selected} />
        <span className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="flex items-center gap-1.5">
            <span className="text-fg truncate text-sm font-medium">
              {step.name.trim() || "Untitled step"}
            </span>
            {problem ? <ProblemDot label="Needs attention" /> : null}
          </span>
          <span
            className={cn(
              "truncate text-xs",
              subject ? "text-fg-2" : "text-fg-3"
            )}
          >
            {subject ? (
              <TemplateText fields={fields} text={subject} />
            ) : (
              "No subject yet"
            )}
          </span>
          {meta.length > 0 ? (
            <span className="text-fg-3 truncate text-xs">
              {meta.join(" · ")}
            </span>
          ) : null}
        </span>
      </button>
      {readOnly ? null : (
        <div className="absolute top-1.5 right-1.5">
          <RowMenu label={`Step ${position} actions`}>
            <DropdownMenuItem
              disabled={position === 1}
              onClick={() => onMove(position - 2)}
            >
              <HugeiconsIcon icon={ArrowUp01Icon} />
              Move up
            </DropdownMenuItem>
            <DropdownMenuItem
              disabled={position === total}
              onClick={() => onMove(position)}
            >
              <HugeiconsIcon icon={ArrowDown01Icon} />
              Move down
            </DropdownMenuItem>
            <DropdownMenuItem
              disabled={total >= STEPS_MAX}
              onClick={onAddAfter}
            >
              <HugeiconsIcon icon={Add01Icon} />
              Add a step after
            </DropdownMenuItem>
            <DropdownMenuItem className="text-error-fg" onClick={onDelete}>
              <HugeiconsIcon icon={Delete02Icon} />
              Delete step
            </DropdownMenuItem>
          </RowMenu>
        </div>
      )}
    </div>
  );
};

/**
 * The sequence as it reads: when it starts, each step with the wait before it, and the way to add
 * the next follow-up (it stays at the bottom of the pane however long the sequence grows). The
 * selected step is the composer's and is always scrolled into view; ↑ and ↓ (Home, End) move
 * through the steps. A step just added grows into place and a deleted one folds away; a step the
 * last refused save named is dotted. Deleting asks first (one that has sent email cannot go:
 * saving says so).
 */
export const Timeline = ({
  added,
  errors,
  fields,
  onAdd,
  onAddAfter,
  onChange,
  onDelete,
  onMove,
  onSelect,
  readOnly,
  selected,
  steps,
}: {
  /** The step just added, which grows into place. */
  added: string | null;
  errors: Readonly<Record<string, string>>;
  fields: readonly FieldName[];
  onAdd: () => void;
  onAddAfter: (index: number) => void;
  onChange: (key: string, patch: Partial<StepDraft>) => void;
  onDelete: (key: string) => void;
  onMove: (from: number, to: number) => void;
  onSelect: (index: number) => void;
  readOnly: boolean;
  selected: number;
  steps: readonly StepDraft[];
}) => {
  const list = useRef<HTMLOListElement>(null);
  // The step as it was when its deletion was asked for: the dialog keeps saying so while it closes.
  const [doomed, setDoomed] = useState<{
    key: string;
    name: string;
    position: number;
    variants: number;
  } | null>(null);
  const [confirming, setConfirming] = useState(false);
  // The step folding away before it leaves the list.
  const [leaving, setLeaving] = useState<string | null>(null);

  useEffect(() => {
    list.current
      ?.querySelector(`[data-step-index="${selected}"]`)
      ?.scrollIntoView({
        behavior: still() ? "auto" : "smooth",
        block: "nearest",
      });
  }, [selected]);

  /** ↑ and ↓ (Home, End) on a step open the one before or after it (the first, the last). */
  const move = (event: KeyboardEvent<HTMLButtonElement>) => {
    const keys: Record<string, number> = {
      ArrowDown: selected + 1,
      ArrowUp: selected - 1,
      End: steps.length - 1,
      Home: 0,
    };
    const next = keys[event.key];
    if (next === undefined || next < 0 || next >= steps.length) {
      return;
    }
    event.preventDefault();
    onSelect(next);
    requestAnimationFrame(() =>
      list.current
        ?.querySelector<HTMLElement>(`[data-step-index="${next}"]`)
        ?.focus()
    );
  };

  return (
    <nav aria-label="Steps" className="flex flex-col">
      <p className="text-fg-3 text-center text-xs">
        Starts when someone is enrolled
      </p>
      <Line />
      <ol className="flex flex-col" ref={list}>
        {steps.map((step, index) => (
          <li
            className={cn(
              "grid grid-rows-[1fr] transition-[grid-template-rows,opacity] duration-200 ease-(--nb-ease-out) data-leaving:grid-rows-[0fr] data-leaving:opacity-0 motion-reduce:transition-none",
              step.key === added &&
                "starting:grid-rows-[0fr] starting:opacity-0"
            )}
            data-leaving={leaving === step.key ? "" : undefined}
            key={step.key}
            onTransitionEnd={(event) => {
              if (
                event.target === event.currentTarget &&
                leaving === step.key
              ) {
                setLeaving(null);
                onDelete(step.key);
              }
            }}
          >
            <div className="-mx-1 -mb-1 min-h-0 overflow-hidden px-1 pb-1">
              {index > 0 ? (
                <Wait
                  error={errors[`steps[${index}].delay_seconds`]}
                  onChange={(patch) => onChange(step.key, patch)}
                  position={index + 1}
                  readOnly={readOnly}
                  step={step}
                />
              ) : null}
              <StepItem
                fields={fields}
                onAddAfter={() => onAddAfter(index)}
                onDelete={() => {
                  setDoomed({
                    key: step.key,
                    name: step.name.trim() || "Untitled step",
                    position: index + 1,
                    variants: step.variants.length,
                  });
                  setConfirming(true);
                }}
                onKeyDown={move}
                onMove={(to) => onMove(index, to)}
                onSelect={() => onSelect(index)}
                position={index + 1}
                problem={hasProblem(errors, `steps[${index}]`)}
                readOnly={readOnly}
                selected={index === selected}
                step={step}
                total={steps.length}
              />
            </div>
          </li>
        ))}
      </ol>
      {readOnly ? null : (
        <div className="bg-surface sticky bottom-0 flex flex-col pb-1">
          <Line />
          <Button
            className="w-full border-dashed"
            disabled={steps.length >= STEPS_MAX}
            onClick={onAdd}
          >
            <HugeiconsIcon icon={Add01Icon} />
            Add follow-up
          </Button>
          {steps.length >= STEPS_MAX ? (
            <p className="text-fg-3 mt-2 text-center text-xs">
              A sequence holds at most {STEPS_MAX} steps.
            </p>
          ) : null}
        </div>
      )}
      <ConfirmDialog
        confirmLabel="Delete step"
        danger
        description={
          doomed
            ? `“${doomed.name}” and its ${plural(doomed.variants, "variant")} leave the sequence when you save. A step that has already sent email can't be deleted; saving says so.`
            : ""
        }
        onConfirm={() => {
          if (!doomed) {
            return;
          }
          if (still()) {
            onDelete(doomed.key);
          } else {
            setLeaving(doomed.key);
          }
        }}
        onOpenChange={setConfirming}
        open={confirming}
        title={doomed ? `Delete step ${doomed.position}?` : "Delete step?"}
      />
    </nav>
  );
};
