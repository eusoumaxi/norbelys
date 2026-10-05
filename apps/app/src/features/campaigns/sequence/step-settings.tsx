import { Collapsible } from "@base-ui/react/collapsible";
import { AiMagicIcon, ArrowRight01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { Allocation, Objective } from "@norbelys/sdk";
import { cn } from "cn";
import { useRef } from "react";
import type { Ref } from "react";

import { Input } from "@/components/ui/input";
import { Segmented } from "@/components/ui/segmented";
import { Select } from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { formatDelay } from "@/features/campaigns/format";
import {
  DAY,
  delayAmount,
  delaySeconds,
  hasProblem,
  snippetNames,
} from "@/features/campaigns/sequence/draft";
import type {
  DelayUnit,
  StepDraft,
  VariantDraft,
} from "@/features/campaigns/sequence/draft";
import { ProblemDot } from "@/features/campaigns/sequence/parts";
import { FormField, SwitchField } from "@/lib/form";
import { formatRate } from "@/lib/format";
import { problemAt } from "@/lib/problem";

/** A whole number typed in a number input, never below zero. */
const whole = (value: string): number =>
  Math.max(0, Math.floor(Number(value) || 0));

const isUnit = (value: string): value is DelayUnit =>
  value === "days" || value === "hours";

/**
 * The wait before a step: a number and its unit, hours or days. Changing the unit keeps the
 * number ("2 days" becomes "2 hours"). `compact` is the chip between two steps of the timeline.
 */
export const DelayInput = ({
  compact = false,
  id,
  invalid,
  label,
  onChange,
  step,
}: {
  compact?: boolean;
  id: string;
  invalid: boolean;
  label: string;
  onChange: (patch: Partial<StepDraft>) => void;
  step: StepDraft;
}) => {
  const amount = delayAmount(step.delay_seconds, step.delay_unit);
  const one = amount === 1;
  return (
    <span className="flex items-center gap-1.5">
      <Input
        aria-invalid={invalid}
        aria-label={label}
        className={cn(
          "tabular-nums",
          compact
            ? "h-6 w-12 [appearance:textfield] px-1.5 text-center text-xs [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
            : "w-20"
        )}
        id={id}
        inputMode="decimal"
        max={step.delay_unit === "days" ? 365 : 365 * 24}
        min={0}
        onChange={(event) =>
          onChange({
            delay_seconds: delaySeconds(
              Number(event.target.value) || 0,
              step.delay_unit
            ),
          })
        }
        step="any"
        type="number"
        value={amount}
      />
      <Select
        className={compact ? "h-6 w-[76px] gap-1 px-2 text-xs" : "w-28"}
        label={`${label}: unit`}
        onChange={(unit) => {
          if (isUnit(unit)) {
            onChange({
              delay_seconds: delaySeconds(amount, unit),
              delay_unit: unit,
            });
          }
        }}
        options={[
          { label: one ? "day" : "days", value: "days" },
          { label: one ? "hour" : "hours", value: "hours" },
        ]}
        value={step.delay_unit}
      />
    </span>
  );
};

const ALLOCATIONS: { label: string; value: Allocation }[] = [
  { label: "Evenly", value: "balanced" },
  { label: "By weight", value: "weighted" },
  { label: "Pick a winner", value: "automatic" },
];

const ALLOCATION_HELP: Record<Allocation, string> = {
  automatic:
    "The variants are tested first; once the results are in, the best one goes to everyone.",
  balanced:
    "Each person gets one variant; every variant goes to as many people.",
  weighted: "Each person gets one variant, in proportion to its weight.",
};

const OBJECTIVES = [
  { label: "Replies", value: "replies" },
  { label: "Clicks", value: "clicks" },
  { label: "Opens", value: "opens" },
];

const isObjective = (value: string): value is Objective =>
  value === "replies" || value === "clicks" || value === "opens";

/**
 * How an `automatic` step names its winner: what the variants are ranked by, how long after the
 * revision is published the results are read, and how many emails each variant sends first.
 */
const WinnerRuleFields = ({
  at,
  errors,
  onChange,
  step,
}: {
  at: string;
  errors: Readonly<Record<string, string>>;
  onChange: (patch: Partial<StepDraft>) => void;
  step: StepDraft;
}) => (
  <div className="grid gap-4 sm:grid-cols-3">
    <FormField
      htmlFor={`${step.key}-objective`}
      label="Best by"
      problem={errors[`${at}.winner_rule.objective`]}
    >
      <Select
        id={`${step.key}-objective`}
        onChange={(value) => {
          if (isObjective(value)) {
            onChange({ objective: value });
          }
        }}
        options={OBJECTIVES}
        value={step.objective}
      />
    </FormField>
    <FormField
      htmlFor={`${step.key}-window`}
      label="Decide after (days)"
      problem={errors[`${at}.winner_rule.observation_window_seconds`]}
    >
      <Input
        aria-invalid={Boolean(
          errors[`${at}.winner_rule.observation_window_seconds`]
        )}
        id={`${step.key}-window`}
        max={365}
        min={1}
        onChange={(event) =>
          onChange({
            observation_window_seconds: whole(event.target.value) * DAY,
          })
        }
        type="number"
        value={Math.round(step.observation_window_seconds / DAY)}
      />
    </FormField>
    <FormField
      htmlFor={`${step.key}-sample`}
      label="Emails per variant first"
      problem={errors[`${at}.winner_rule.minimum_sample`]}
    >
      <Input
        aria-invalid={Boolean(errors[`${at}.winner_rule.minimum_sample`])}
        id={`${step.key}-sample`}
        max={1_000_000}
        min={1}
        onChange={(event) =>
          onChange({ minimum_sample: whole(event.target.value) })
        }
        type="number"
        value={step.minimum_sample}
      />
    </FormField>
  </div>
);

/** Each variant's name and, when the step sends by weight, its weight and the share it gives. */
const VariantRows = ({
  at,
  errors,
  onChange,
  step,
}: {
  at: string;
  errors: Readonly<Record<string, string>>;
  onChange: (patch: Partial<StepDraft>) => void;
  step: StepDraft;
}) => {
  const weighted = step.allocation === "weighted";
  const total = step.variants.reduce((sum, variant) => sum + variant.weight, 0);
  const set = (key: string, patch: Partial<VariantDraft>) =>
    onChange({
      variants: step.variants.map((variant) =>
        variant.key === key ? { ...variant, ...patch } : variant
      ),
    });
  return (
    <ul className="flex flex-col gap-2">
      {step.variants.map((variant, index) => {
        const path = `${at}.variants[${index}]`;
        const problem = errors[`${path}.name`] ?? errors[`${path}.weight`];
        return (
          <li className="flex flex-col gap-1" key={variant.key}>
            <div className="flex items-center gap-2">
              <Input
                aria-invalid={Boolean(errors[`${path}.name`])}
                aria-label={`Variant ${index + 1} name`}
                className="w-40"
                maxLength={200}
                onChange={(event) =>
                  set(variant.key, { name: event.target.value })
                }
                value={variant.name}
              />
              {weighted ? (
                <>
                  <Input
                    aria-invalid={Boolean(errors[`${path}.weight`])}
                    aria-label={`Weight of variant ${variant.name}`}
                    className="w-20 tabular-nums"
                    max={100}
                    min={1}
                    onChange={(event) =>
                      set(variant.key, { weight: whole(event.target.value) })
                    }
                    type="number"
                    value={variant.weight}
                  />
                  <span className="text-fg-3 text-xs tabular-nums">
                    {formatRate(variant.weight, total) ?? "No share"}
                  </span>
                </>
              ) : null}
            </div>
            {problem ? (
              <p className="text-error-fg text-xs">{problem}</p>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
};

/** The settings whose problems open the area: the API's paths under the step's own fields. */
const SETTINGS =
  /^steps\[\d+\]\.(?:delay_seconds|same_thread|allocation|personalisation_prompt|winner_rule|variants\[\d+\]\.(?:name|weight))/u;

/** Whether the last refused save named one of the step's settings. */
const settingsProblem = (
  errors: Readonly<Record<string, string>>,
  at: string
): boolean =>
  Object.keys(errors).some(
    (path) => path.startsWith(`${at}.`) && SETTINGS.test(path)
  );

/** The settings in one line, for the closed area: what differs from writing one email by hand. */
const summary = (step: StepDraft, position: number): string => {
  const thread = step.same_thread ? "Same thread" : "New thread";
  const sent: Record<Allocation, string> = {
    automatic: "Picks a winner",
    balanced: "Variants sent evenly",
    weighted: "Variants sent by weight",
  };
  return [
    position > 1 ? thread : null,
    step.personalised ? "Personalized with AI" : null,
    step.variants.length > 1 ? sent[step.allocation] : null,
  ]
    .filter((part) => part !== null)
    .join(" · ");
};

/**
 * A step's less common settings, closed until opened: the wait (after the first step), whether
 * it replies in the same thread, AI personalisation and its prompt, and, with two variants or
 * more, how they are sent. A refused save that names one of them opens it.
 */
export const StepSettings = ({
  at,
  errors,
  onChange,
  onOpenChange,
  open,
  position,
  promptRef,
  readOnly,
  step,
}: {
  at: string;
  errors: Readonly<Record<string, string>>;
  onChange: (patch: Partial<StepDraft>) => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  position: number;
  promptRef: Ref<HTMLTextAreaElement>;
  readOnly: boolean;
  step: StepDraft;
}) => {
  const panel = useRef<HTMLDivElement>(null);
  const problem = settingsProblem(errors, at);
  const line = summary(step, position);
  const silentAi =
    step.personalised &&
    step.personalisation_prompt.trim() !== "" &&
    snippetNames(step).length === 0;
  // The winner rule's inputs show only for a test that picks a winner; a problem with the rule
  // otherwise still needs a place.
  const ruleShown =
    (step.variants.length > 1 && step.allocation === "automatic") ||
    !hasProblem(errors, `${at}.winner_rule`);
  return (
    <Collapsible.Root
      className="border-line border-t"
      onOpenChange={(next) => {
        onOpenChange(next);
        if (next) {
          // The settings open below the email: bring them into view once they have grown.
          setTimeout(() => {
            panel.current?.scrollIntoView({
              behavior: window.matchMedia("(prefers-reduced-motion: reduce)")
                .matches
                ? "auto"
                : "smooth",
              block: "nearest",
            });
          }, 220);
        }
      }}
      open={open || problem}
    >
      <Collapsible.Trigger className="group text-fg focus-visible:outline-focus flex w-full cursor-pointer items-center gap-2 py-3 text-left text-sm font-semibold outline-none focus-visible:outline-1">
        <HugeiconsIcon
          className="text-icon size-4 shrink-0 transition-transform duration-200 group-data-panel-open:rotate-90 motion-reduce:transition-none"
          icon={ArrowRight01Icon}
        />
        Step settings
        {line ? (
          <span className="text-fg-3 min-w-0 truncate font-normal">{line}</span>
        ) : null}
        {problem ? <ProblemDot label="A setting needs attention" /> : null}
      </Collapsible.Trigger>
      <Collapsible.Panel
        className="h-(--collapsible-panel-height) overflow-hidden transition-[height] duration-200 ease-(--nb-ease-out) data-ending-style:h-0 data-starting-style:h-0 motion-reduce:transition-none"
        ref={panel}
      >
        <fieldset
          className="flex min-w-0 flex-col gap-6 pt-1 pb-1 pl-6"
          disabled={readOnly}
        >
          <legend className="sr-only">Step settings</legend>
          {position > 1 ? (
            <FormField
              description={`${formatDelay(step.delay_seconds)} after step ${position - 1} is sent, then at the next time the send window allows.`}
              htmlFor={`${step.key}-wait`}
              label="Wait before sending"
              problem={errors[`${at}.delay_seconds`]}
            >
              <DelayInput
                id={`${step.key}-wait`}
                invalid={Boolean(errors[`${at}.delay_seconds`])}
                label="Wait before sending"
                onChange={onChange}
                step={step}
              />
            </FormField>
          ) : null}
          {position > 1 ? (
            <div className="flex flex-col gap-1">
              <SwitchField
                checked={step.same_thread}
                description={
                  step.same_thread
                    ? "Sent as a reply to the previous email, in the same conversation. Keep its subject, with “Re: ”, so inboxes thread them together."
                    : "Starts a new conversation, with its own subject."
                }
                id={`${step.key}-thread`}
                label="Reply in the same thread"
                onChange={(checked) => onChange({ same_thread: checked })}
              />
              {errors[`${at}.same_thread`] ? (
                <p className="text-error-fg pl-10 text-xs">
                  {errors[`${at}.same_thread`]}
                </p>
              ) : null}
            </div>
          ) : null}
          <div className="flex flex-col gap-3">
            <SwitchField
              checked={step.personalised}
              description="Norbe writes a line for each person, such as an opening line, from your instructions and what you know about them. Put it in the email from Personalize. When it can't, the email goes without it."
              id={`${step.key}-ai`}
              label={
                <span className="flex items-center gap-1.5">
                  <HugeiconsIcon
                    className="text-icon size-4"
                    icon={AiMagicIcon}
                  />
                  Personalize with AI
                </span>
              }
              onChange={(checked) => onChange({ personalised: checked })}
            />
            {step.personalised ? (
              <FormField
                className="pl-10"
                description={
                  silentAi ? (
                    <span className="text-warning">
                      This email reads no AI snippet yet: insert one from
                      Personalize.
                    </span>
                  ) : undefined
                }
                htmlFor={`${step.key}-prompt`}
                label="Instructions for Norbe"
                problem={errors[`${at}.personalisation_prompt`]}
              >
                <Textarea
                  aria-invalid={Boolean(errors[`${at}.personalisation_prompt`])}
                  className="min-h-20"
                  id={`${step.key}-prompt`}
                  maxLength={4000}
                  onChange={(event) =>
                    onChange({ personalisation_prompt: event.target.value })
                  }
                  placeholder="Write one friendly opening line about their company, using what their fields say."
                  ref={promptRef}
                  value={step.personalisation_prompt}
                />
              </FormField>
            ) : null}
          </div>
          {step.variants.length > 1 ? (
            <div className="flex flex-col gap-3">
              <FormField
                description={ALLOCATION_HELP[step.allocation]}
                label="Send the variants"
                problem={errors[`${at}.allocation`]}
              >
                <Segmented<Allocation>
                  label="Send the variants"
                  onChange={(allocation) => onChange({ allocation })}
                  options={ALLOCATIONS}
                  value={step.allocation}
                />
              </FormField>
              <VariantRows
                at={at}
                errors={errors}
                onChange={onChange}
                step={step}
              />
              {step.allocation === "automatic" ? (
                <WinnerRuleFields
                  at={at}
                  errors={errors}
                  onChange={onChange}
                  step={step}
                />
              ) : null}
            </div>
          ) : null}
          {ruleShown ? null : (
            <p className="text-error-fg text-xs">
              {problemAt(errors, `${at}.winner_rule`)}
            </p>
          )}
        </fieldset>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
};
