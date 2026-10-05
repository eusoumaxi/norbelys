import {
  Add01Icon,
  ArrowDown01Icon,
  ArrowUp01Icon,
  Crown02Icon,
  Delete02Icon,
  MailSend01Icon,
  UnfoldMoreIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { CampaignObject, PersonObject } from "@norbelys/sdk";
import { cn } from "cn";
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { FieldError } from "@/components/ui/field";
import { Segmented } from "@/components/ui/segmented";
import { Tabs, TabsList, TabsTab } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import {
  hasProblem,
  newVariant,
  stepErrors,
  VARIANTS_MAX,
} from "@/features/campaigns/sequence/draft";
import type {
  StepDraft,
  VariantDraft,
} from "@/features/campaigns/sequence/draft";
import { EmailEditor } from "@/features/campaigns/sequence/email-editor";
import { EmailPreview } from "@/features/campaigns/sequence/email-preview";
import { scroller } from "@/features/campaigns/sequence/fit";
import {
  enter,
  ProblemDot,
  StepNumber,
  TemplateText,
} from "@/features/campaigns/sequence/parts";
import { SendTestDialog } from "@/features/campaigns/sequence/send-test-dialog";
import { StepSettings } from "@/features/campaigns/sequence/step-settings";
import type { FieldName } from "@/features/messages/merge-tags";
import { formatDateTime } from "@/lib/format";

export type View = "edit" | "preview";

const VIEWS = [
  { label: "Edit", value: "edit" as const },
  { label: "Preview", value: "preview" as const },
];

/** More variants than this, and a list of them all joins the tabs. */
const TABS_ALONE = 6;

/** A variant's name in full: "Variant A" for a letter, the name itself when it was given one. */
const variantName = (variant: VariantDraft): string => {
  const name = variant.name.trim();
  if (!name) {
    return "Untitled variant";
  }
  return /^[A-Z]{1,2}$/u.test(name) ? `Variant ${name}` : name;
};

/** Who chose the step's winner, and when: everyone gets it until the step changes. */
const WinnerNote = ({ step }: { step: StepDraft }) => {
  const { winner } = step;
  if (!winner) {
    return null;
  }
  const variant = step.variants.find((v) => v.id === winner.variant_id);
  const how =
    winner.selected_by === "automatic" ? "automatically" : "by a person";
  return (
    <p className="text-fg-2 text-xs">
      {variant ? variantName(variant) : "A variant no longer offered"} won on{" "}
      {formatDateTime(winner.selected_at)}, chosen {how}: everyone gets it now.
      Changing this step starts a new test.
    </p>
  );
};

/** Every variant of a step in a list, for a step with too many to read as tabs. */
const VariantList = ({
  fields,
  onSelect,
  selected,
  step,
}: {
  fields: readonly FieldName[];
  onSelect: (key: string) => void;
  selected: string;
  step: StepDraft;
}) => (
  <DropdownMenu>
    <DropdownMenuTrigger
      aria-label={`All ${step.variants.length} variants`}
      render={<Button size="s" variant="tertiary" />}
    >
      <HugeiconsIcon icon={UnfoldMoreIcon} />
      <span className="tabular-nums">{step.variants.length}</span>
    </DropdownMenuTrigger>
    <DropdownMenuContent align="end" className="w-80">
      <DropdownMenuGroup>
        <DropdownMenuRadioGroup
          onValueChange={(value: unknown) => {
            if (typeof value === "string") {
              onSelect(value);
            }
          }}
          value={selected}
        >
          {step.variants.map((variant) => (
            <DropdownMenuRadioItem key={variant.key} value={variant.key}>
              <span className="max-w-24 min-w-6 shrink-0 truncate">
                {variant.name}
              </span>
              <span className="text-fg-3 min-w-0 flex-1 truncate font-normal">
                {variant.subject ? (
                  <TemplateText fields={fields} text={variant.subject} />
                ) : (
                  "No subject yet"
                )}
              </span>
            </DropdownMenuRadioItem>
          ))}
        </DropdownMenuRadioGroup>
      </DropdownMenuGroup>
    </DropdownMenuContent>
  </DropdownMenu>
);

/**
 * The variants of a step as one row of tabs that scrolls sideways (the open one always in view),
 * the winner crowned and a problem dotted; with many variants, a list of them all beside it. The
 * open tab can be deleted while the step has another; "Add variant" starts a copy of it.
 */
const VariantTabs = ({
  at,
  errors,
  fields,
  onAdd,
  onRemove,
  onSelect,
  readOnly,
  selected,
  step,
}: {
  at: string;
  errors: Readonly<Record<string, string>>;
  fields: readonly FieldName[];
  onAdd: () => void;
  onRemove: (variant: VariantDraft) => void;
  onSelect: (key: string) => void;
  readOnly: boolean;
  selected: string;
  step: StepDraft;
}) => {
  const strip = useRef<HTMLDivElement>(null);
  useEffect(() => {
    strip.current
      ?.querySelector(`[data-variant="${selected}"]`)
      ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [selected]);
  return (
    <div className="border-line flex items-end gap-4 border-b">
      <Tabs
        className="min-w-0"
        onValueChange={(value: unknown) => {
          if (typeof value === "string") {
            onSelect(value);
          }
        }}
        value={selected}
      >
        <TabsList className="scrollbar-thin gap-4 border-b-0" ref={strip}>
          {step.variants.map((variant, index) => {
            const winner = Boolean(
              variant.id && step.winner?.variant_id === variant.id
            );
            return (
              <span
                className="flex items-start gap-0.5"
                data-variant={variant.key}
                key={variant.key}
              >
                <TabsTab value={variant.key}>
                  <span className="flex h-5 max-w-40 items-center gap-1.5">
                    <span className="truncate">{variantName(variant)}</span>
                    {winner ? (
                      <>
                        <HugeiconsIcon
                          aria-hidden
                          className="text-success size-3.5"
                          icon={Crown02Icon}
                        />
                        <span className="sr-only">Winner</span>
                      </>
                    ) : null}
                    {hasProblem(errors, `${at}.variants[${index}]`) ? (
                      <ProblemDot label="Needs attention" />
                    ) : null}
                  </span>
                </TabsTab>
                {readOnly ||
                step.variants.length < 2 ||
                variant.key !== selected ? null : (
                  <button
                    aria-label={`Delete ${variantName(variant)}`}
                    className="text-fg-3 hover:text-fg hover:bg-hover focus-visible:outline-focus flex size-5 cursor-pointer items-center justify-center rounded-xs outline-none focus-visible:outline-1"
                    onClick={() => onRemove(variant)}
                    type="button"
                  >
                    <HugeiconsIcon className="size-3.5" icon={Delete02Icon} />
                  </button>
                )}
              </span>
            );
          })}
        </TabsList>
      </Tabs>
      <div className="ml-auto flex shrink-0 items-center gap-1 pb-1.5">
        {step.variants.length > TABS_ALONE ? (
          <VariantList
            fields={fields}
            onSelect={onSelect}
            selected={selected}
            step={step}
          />
        ) : null}
        {readOnly || step.variants.length >= VARIANTS_MAX ? null : (
          <Button onClick={onAdd} size="s" variant="tertiary">
            <HugeiconsIcon icon={Add01Icon} />
            Add variant
          </Button>
        )}
      </div>
    </div>
  );
};

/** Opens the test send; a variant not saved yet has nothing to send, and says so on hover. */
const TestButton = ({
  onClick,
  saved,
}: {
  onClick: () => void;
  saved: boolean;
}) => {
  const button = (
    <Button disabled={!saved} onClick={onClick}>
      <HugeiconsIcon icon={MailSend01Icon} />
      Send a test
    </Button>
  );
  if (saved) {
    return button;
  }
  return (
    <Tooltip>
      <TooltipTrigger render={<span />}>{button}</TooltipTrigger>
      <TooltipContent>
        Save the sequence first: a test sends the saved email.
      </TooltipContent>
    </Tooltip>
  );
};

/** Whether a variant holds anything a person wrote: deleting it then asks first. */
const written = (variant: VariantDraft): boolean =>
  variant.subject.trim() !== "" ||
  variant.preheader.trim() !== "" ||
  variant.body.html.trim() !== "";

/**
 * The selected step, written as an email: its name and the way to the previous or next step,
 * its variants as tabs, the open variant's subject and body (or their preview, filled in for a
 * person), a test send of the saved variant, and the step's less common settings, closed until
 * opened. What changes eases in once the page has loaded (`moving`).
 */
export const Composer = ({
  autoFocus,
  base,
  campaign,
  canTest,
  errors,
  fields,
  index,
  moving,
  onChange,
  onPerson,
  onSelectVariant,
  onStep,
  onView,
  person,
  readOnly,
  save,
  step,
  total,
  variant,
  view,
}: {
  /** Puts the cursor in the body once the step shows: it was just added. */
  autoFocus: boolean;
  /** The campaign as saved, which a test send reads. */
  base: CampaignObject;
  campaign: CampaignObject;
  /** Whether the person may send a test (a member who writes, a campaign not archived). */
  canTest: boolean;
  errors: Readonly<Record<string, string>>;
  fields: readonly FieldName[];
  index: number;
  moving: boolean;
  onChange: (patch: Partial<StepDraft>) => void;
  onPerson: (person: PersonObject | null) => void;
  onSelectVariant: (key: string) => void;
  /** Opens the step at this position. */
  onStep: (index: number) => void;
  onView: (view: View) => void;
  person: PersonObject | null;
  readOnly: boolean;
  /** The save state, at the end of the top row; none for a read-only editor. */
  save?: ReactNode;
  step: StepDraft;
  total: number;
  variant: VariantDraft;
  view: View;
}) => {
  const [settingsOpen, setSettingsOpen] = useState(false);
  // The variant whose deletion was asked for: the dialog keeps naming it while it closes.
  const [removing, setRemoving] = useState<VariantDraft | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [testing, setTesting] = useState(false);
  const prompt = useRef<HTMLTextAreaElement>(null);
  // The top row sticks to the top of what scrolls; its hairline shows once the content passes under.
  const sentinel = useRef<HTMLDivElement>(null);
  const [stuck, setStuck] = useState(false);
  useEffect(() => {
    const mark = sentinel.current;
    const root = mark ? scroller(mark) : null;
    if (!mark) {
      return;
    }
    const observer = new IntersectionObserver(
      ([entry]) =>
        setStuck(
          entry !== undefined &&
            !entry.isIntersecting &&
            entry.boundingClientRect.top < (entry.rootBounds?.top ?? 0)
        ),
      { root }
    );
    observer.observe(mark);
    return () => observer.disconnect();
  }, []);
  const at = `steps[${index}]`;
  const variantIndex = step.variants.findIndex((v) => v.key === variant.key);
  const variantAt = `${at}.variants[${variantIndex}]`;
  const general = [
    ...stepErrors(errors, at),
    errors[variantAt],
    errors[`${variantAt}.id`],
    // With one variant there is no list of names and weights in the settings to show these.
    ...(step.variants.length < 2
      ? [errors[`${variantAt}.name`], errors[`${variantAt}.weight`]]
      : []),
  ].filter((message) => message !== undefined);
  const setVariant = (patch: Partial<VariantDraft>) =>
    onChange({
      variants: step.variants.map((v) =>
        v.key === variant.key ? { ...v, ...patch } : v
      ),
    });
  const remove = (gone: VariantDraft) => {
    onChange({ variants: step.variants.filter((v) => v.key !== gone.key) });
  };
  const enableAi = () => {
    onChange({ personalised: true });
    setSettingsOpen(true);
    requestAnimationFrame(() => prompt.current?.focus());
  };

  return (
    <div className="flex flex-col gap-4">
      <div aria-hidden className="-mb-4 h-px" ref={sentinel} />
      <div
        className={cn(
          "bg-surface sticky top-0 z-10 -mt-px flex flex-wrap items-center gap-x-4 gap-y-2 border-b py-2 transition-colors duration-120",
          stuck ? "border-line" : "border-transparent"
        )}
      >
        <div className="flex min-w-0 flex-[1_1_240px] items-center gap-2">
          <StepNumber position={index + 1} selected />
          <input
            aria-invalid={Boolean(errors[`${at}.name`])}
            aria-label={`Name of step ${index + 1}`}
            className="text-fg placeholder:text-fg-3 hover:border-line focus-visible:border-focus aria-invalid:border-error-line h-9 min-w-0 flex-1 rounded-sm border border-transparent bg-transparent px-1 text-xl font-semibold transition-colors outline-none read-only:hover:border-transparent"
            maxLength={200}
            onChange={(event) => onChange({ name: event.target.value })}
            placeholder="Name this step"
            readOnly={readOnly}
            value={step.name}
          />
          <span className="flex shrink-0 items-center">
            <Button
              aria-label="Previous step"
              disabled={index === 0}
              onClick={() => onStep(index - 1)}
              size="icon-s"
              variant="tertiary"
            >
              <HugeiconsIcon icon={ArrowUp01Icon} />
            </Button>
            <Button
              aria-label="Next step"
              disabled={index === total - 1}
              onClick={() => onStep(index + 1)}
              size="icon-s"
              variant="tertiary"
            >
              <HugeiconsIcon icon={ArrowDown01Icon} />
            </Button>
          </span>
        </div>
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <Segmented<View>
            label="Show"
            onChange={onView}
            options={VIEWS}
            value={view}
          />
          {canTest ? (
            <TestButton
              onClick={() => setTesting(true)}
              saved={variant.id !== undefined}
            />
          ) : null}
          {/* On a phone the save state takes a line of its own, on the right. */}
          {save ? <div className="max-sm:ml-auto">{save}</div> : null}
        </div>
      </div>
      {errors[`${at}.name`] ? (
        <FieldError className="-mt-2">{errors[`${at}.name`]}</FieldError>
      ) : null}
      <VariantTabs
        at={at}
        errors={errors}
        fields={fields}
        onAdd={() => {
          const added = newVariant(step.variants, variant);
          onChange({ variants: [...step.variants, added] });
          onSelectVariant(added.key);
        }}
        onRemove={(gone) => {
          if (written(gone)) {
            setRemoving(gone);
            setConfirming(true);
          } else {
            remove(gone);
          }
        }}
        onSelect={onSelectVariant}
        readOnly={readOnly}
        selected={variant.key}
        step={step}
      />
      <WinnerNote step={step} />
      {general.map((message) => (
        <FieldError key={message}>{message}</FieldError>
      ))}
      {view === "preview" ? (
        <EmailPreview
          campaign={campaign}
          className={enter(moving)}
          fields={fields}
          key={`${variant.key}:preview`}
          onPerson={onPerson}
          person={person}
          position={index + 1}
          step={step}
          variant={variant}
        />
      ) : (
        <div className={enter(moving) ?? undefined} key={`${variant.key}:edit`}>
          <EmailEditor
            at={variantAt}
            autoFocus={autoFocus}
            errors={errors}
            fields={fields}
            onChange={setVariant}
            onEnableAi={enableAi}
            readOnly={readOnly}
            step={step}
            variant={variant}
          />
          <p className="text-fg-3 mt-2 text-xs">
            The sender&apos;s signature and the unsubscribe link are added when
            it is sent.
          </p>
        </div>
      )}
      <StepSettings
        at={at}
        errors={errors}
        onChange={onChange}
        onOpenChange={setSettingsOpen}
        open={settingsOpen}
        position={index + 1}
        promptRef={prompt}
        readOnly={readOnly}
        step={step}
      />
      <ConfirmDialog
        confirmLabel="Delete variant"
        danger
        description="Its subject and body leave this step when you save. Emails already sent from it keep it."
        onConfirm={() => {
          if (removing) {
            remove(removing);
          }
        }}
        onOpenChange={setConfirming}
        open={confirming}
        title={`Delete ${removing ? variantName(removing) : "variant"}?`}
      />
      {canTest && variant.id ? (
        <SendTestDialog
          base={base}
          initialPerson={person}
          onOpenChange={setTesting}
          open={testing}
          step={step}
          variant={variant}
        />
      ) : null}
    </div>
  );
};
