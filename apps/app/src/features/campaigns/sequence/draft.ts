import type {
  Allocation,
  CampaignObject,
  Objective,
  StepInput,
  StepObject,
  VariantObject,
  WinnerObject,
} from "@norbelys/sdk";

import { bodyDraftOf, EMPTY_BODY } from "@/features/messages/body-editor";
import type { BodyDraft } from "@/features/messages/body-editor";
import { problemAt } from "@/lib/problem";

/** A variant as the editor holds it: the API's fields as plain strings while they are typed. */
export interface VariantDraft {
  /** React's key: the variant's id, or a fresh one for a variant not saved yet. */
  key: string;
  id?: string;
  /** The version of its content the step's revision offers (unknown for a new variant). */
  version?: number;
  name: string;
  subject: string;
  preheader: string;
  /** The body as written; its `html` is what is saved. */
  body: BodyDraft;
  weight: number;
}

/** The unit a wait is shown and typed in. */
export type DelayUnit = "hours" | "days";

/** A step as the editor holds it. */
export interface StepDraft {
  key: string;
  id?: string;
  revision?: number;
  winner?: WinnerObject | null;
  name: string;
  delay_seconds: number;
  /** How the wait is shown; not saved (the API holds seconds). */
  delay_unit: DelayUnit;
  same_thread: boolean;
  allocation: Allocation;
  objective: Objective;
  observation_window_seconds: number;
  minimum_sample: number;
  /** Whether the step is personalised with AI: its prompt is saved only while this is on. */
  personalised: boolean;
  personalisation_prompt: string;
  variants: VariantDraft[];
}

/** The most steps a campaign has, and variants a step offers, as the API allows. */
export const STEPS_MAX = 50;
export const VARIANTS_MAX = 50;

const LETTERS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

const HOUR = 3600;
export const DAY = 86_400;
/** The longest wait the API takes: a year. */
const DELAY_MAX = 365 * DAY;
const UNIT: Record<DelayUnit, number> = { days: DAY, hours: HOUR };

/** The unit a saved wait reads best in: days when it is whole days (or none), else hours. */
const delayUnitOf = (seconds: number): DelayUnit =>
  seconds % DAY === 0 ? "days" : "hours";

/** A wait as a number of `unit`, to two decimals at most (a wait set through the API may be odd). */
export const delayAmount = (seconds: number, unit: DelayUnit): number =>
  Math.round((seconds / UNIT[unit]) * 100) / 100;

/** `amount` of `unit` in seconds, within the API's bounds: no less than none, no more than a year. */
export const delaySeconds = (amount: number, unit: DelayUnit): number =>
  Math.min(DELAY_MAX, Math.max(0, Math.round(amount * UNIT[unit])));

const variantDraft = (variant: VariantObject): VariantDraft => ({
  body: bodyDraftOf(variant.html ?? ""),
  id: variant.id,
  key: variant.id,
  name: variant.name,
  preheader: variant.preheader ?? "",
  subject: variant.subject,
  version: variant.version,
  weight: variant.weight,
});

const stepDraft = (step: StepObject): StepDraft => ({
  allocation: step.allocation,
  delay_seconds: step.delay_seconds,
  delay_unit: delayUnitOf(step.delay_seconds),
  id: step.id,
  key: step.id,
  minimum_sample: step.winner_rule.minimum_sample,
  name: step.name,
  objective: step.winner_rule.objective,
  observation_window_seconds: step.winner_rule.observation_window_seconds,
  personalisation_prompt: step.personalisation_prompt ?? "",
  personalised: Boolean(step.personalisation_prompt),
  revision: step.revision,
  same_thread: step.same_thread,
  variants: step.variants.map(variantDraft),
  winner: step.winner,
});

/** The campaign's steps, in order, ready to edit. */
export const fromCampaign = (campaign: CampaignObject): StepDraft[] =>
  campaign.steps.map(stepDraft);

/**
 * A new variant, named by the first letter its step does not use yet (then "Variant 27" and on,
 * as the API names them). It starts as a copy of
 * `from` when given: a test usually changes one thing, such as the subject.
 */
export const newVariant = (
  taken: readonly VariantDraft[],
  from?: VariantDraft
): VariantDraft => {
  const names = new Set(taken.map((variant) => variant.name));
  const letter =
    [...LETTERS].find((candidate) => !names.has(candidate)) ??
    `Variant ${taken.length + 1}`;
  return {
    body: from?.body ?? EMPTY_BODY,
    key: crypto.randomUUID(),
    name: letter,
    preheader: from?.preheader ?? "",
    subject: from?.subject ?? "",
    weight: 1,
  };
};

/** `Re: ` and a subject, as a reply in the same conversation carries it (once). */
const replySubject = (subject: string): string =>
  /^re:/iu.test(subject) ? subject : `Re: ${subject}`;

/**
 * A new step with one empty variant. The first starts the conversation; a follow-up waits two
 * days, answers in the same thread and starts from `previous`'s subject as a reply, so inboxes
 * keep the conversation together.
 */
export const newStep = (position: number, previous?: StepDraft): StepDraft => {
  const subject = previous?.variants[0]?.subject.trim() ?? "";
  const variant = newVariant([]);
  return {
    allocation: "balanced",
    delay_seconds: position > 1 ? 2 * DAY : 0,
    delay_unit: "days",
    key: crypto.randomUUID(),
    minimum_sample: 100,
    name: position > 1 ? `Follow-up ${position - 1}` : "Introduction",
    objective: "replies",
    observation_window_seconds: 7 * DAY,
    personalisation_prompt: "",
    personalised: false,
    same_thread: true,
    variants: [
      {
        ...variant,
        subject: position > 1 && subject ? replySubject(subject) : "",
      },
    ],
  };
};

/**
 * The ordered list `campaigns.update` takes: every step and variant with its id when it has
 * one, so unchanged ones keep their revision and version, and new ones are created.
 */
export const toInput = (steps: readonly StepDraft[]): StepInput[] =>
  steps.map((step) => ({
    allocation: step.allocation,
    delay_seconds: step.delay_seconds,
    id: step.id,
    name: step.name.trim(),
    personalisation_prompt:
      step.personalised && step.personalisation_prompt.trim()
        ? step.personalisation_prompt
        : null,
    same_thread: step.same_thread,
    variants: step.variants.map((variant) => ({
      html: variant.body.html,
      id: variant.id,
      name: variant.name.trim(),
      preheader: variant.preheader || null,
      subject: variant.subject,
      weight: variant.weight,
    })),
    winner_rule: {
      minimum_sample: step.minimum_sample,
      objective: step.objective,
      observation_window_seconds: step.observation_window_seconds,
    },
  }));

/**
 * `input` as it is sent: every variant still named, but one that is unchanged from `saved` (the
 * campaign's own steps as `toInput` writes them) by its id alone, which the API keeps as it is.
 * A save then carries only what was edited, so a large sequence stays within the request limit.
 */
export const slimInput = (
  input: readonly StepInput[],
  saved: readonly StepInput[]
): StepInput[] => {
  const before = new Map(
    saved.flatMap((step) =>
      (step.variants ?? []).map((variant) => [
        variant.id ?? "",
        JSON.stringify(variant),
      ])
    )
  );
  return input.map((step) => ({
    ...step,
    variants: step.variants?.map((variant) =>
      variant.id && before.get(variant.id) === JSON.stringify(variant)
        ? { id: variant.id }
        : variant
    ),
  }));
};

/** `list` with the item at `from` moved to `to`. */
export const moved = <T>(list: readonly T[], from: number, to: number): T[] => {
  const item = list[from];
  if (item === undefined || to < 0 || to >= list.length) {
    return [...list];
  }
  const rest = list.toSpliced(from, 1);
  return rest.toSpliced(to, 0, item);
};

/** Whether a variant's content differs from the version saved in `base`: a test sends the saved one. */
export const unsavedContent = (
  variant: VariantDraft,
  base: CampaignObject
): boolean => {
  const saved = base.steps
    .flatMap((step) => step.variants)
    .find((candidate) => candidate.id === variant.id);
  return (
    !saved ||
    saved.subject !== variant.subject ||
    (saved.preheader ?? "") !== variant.preheader ||
    (saved.html ?? "") !== variant.body.html
  );
};

/** The names of the AI snippets a template reads: `{{ variables.opener }}` reads `opener`. */
const SNIPPET = /\bvariables\.(?<name>[a-z][a-z0-9_]{0,63})\b/gu;

/** The AI snippets a step's variants read, in the order they are first used. */
export const snippetNames = (step: StepDraft): string[] => [
  ...new Set(
    step.variants.flatMap((variant) =>
      [variant.subject, variant.preheader, variant.body.html].flatMap((text) =>
        [...text.matchAll(SNIPPET)].flatMap((match) => match.groups?.name ?? [])
      )
    )
  ),
];

/** The paths of the API's field errors the editor shows beside a step's or a variant's input. */
const PLACED =
  /^steps\[\d+\](?:\.(?:id|name|delay_seconds|same_thread|allocation|personalisation_prompt|winner_variant_id|variants|winner_rule(?:\.\w+)?)|\.variants\[\d+\](?:\.(?:id|name|subject|preheader|html|weight))?)?$/u;

/** The field errors no input shows: they go in the summary above the steps. */
export const unplaced = (
  errors: Readonly<Record<string, string>>
): [string, string][] =>
  Object.entries(errors).filter(([path]) => !PLACED.test(path));

/** A step's errors that belong to no single input: the step itself, its id, its variant list. */
export const stepErrors = (
  errors: Readonly<Record<string, string>>,
  at: string
): string[] =>
  [at, `${at}.id`, `${at}.variants`, `${at}.winner_variant_id`]
    .map((path) => errors[path])
    .filter((message) => message !== undefined);

/** Whether the API's last refusal named anything at `at` or under it (`steps[1]`, `steps[1].variants[0]`). */
export const hasProblem = (
  errors: Readonly<Record<string, string>>,
  at: string
): boolean => problemAt(errors, at) !== undefined;
