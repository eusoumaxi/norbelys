import { Add01Icon, Mail01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type {
  CampaignObject,
  PersonObject,
  StepInput,
  UpdateCampaign,
} from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useRef, useState } from "react";

import { EmptyPanel } from "@/components/data-table";
import { Button } from "@/components/ui/button";
import {
  ArchivedNote,
  SaveBar,
  SaveProblem,
  useCampaignEditor,
} from "@/features/campaigns/campaign-editor";
import { campaignEditable, useCampaign } from "@/features/campaigns/queries";
import { Composer } from "@/features/campaigns/sequence/composer";
import type { View } from "@/features/campaigns/sequence/composer";
import {
  fromCampaign,
  moved,
  newStep,
  slimInput,
  toInput,
  unplaced,
} from "@/features/campaigns/sequence/draft";
import type { StepDraft } from "@/features/campaigns/sequence/draft";
import { fitToPanel } from "@/features/campaigns/sequence/fit";
import { Timeline } from "@/features/campaigns/sequence/timeline";
import type { FieldName } from "@/features/messages/merge-tags";
import { fieldsQuery } from "@/features/people/queries";
import { fieldProblems } from "@/lib/problem";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The saved sequence as `campaigns.update` takes it, read once per version of the campaign. */
const saved = new WeakMap<CampaignObject, string>();

const savedInput = (base: CampaignObject): string => {
  const known = saved.get(base);
  if (known !== undefined) {
    return known;
  }
  const input = JSON.stringify(toInput(fromCampaign(base)));
  saved.set(base, input);
  return input;
};

/** The whole ordered list of steps, when it differs from the campaign's. */
const sequenceChanges = (
  steps: StepDraft[],
  base: CampaignObject
): UpdateCampaign | null => {
  const input: StepInput[] = toInput(steps);
  return JSON.stringify(input) === savedInput(base)
    ? null
    : { steps: slimInput(input, toInput(fromCampaign(base))) };
};

/**
 * What is selected: a key, and the position to fall back on when the key is gone (a saved step
 * or variant takes its id as its key, a removed one leaves its place to the next).
 */
interface Selection {
  index: number;
  key: string;
}

const resolve = (keys: readonly string[], selection: Selection): number => {
  const found = keys.indexOf(selection.key);
  return found === -1
    ? Math.max(0, Math.min(selection.index, keys.length - 1))
    : found;
};

/** Where the two panes stand side by side, and the timeline is no longer above the composer. */
const SIDE_BY_SIDE = "(min-width: 64rem)";

/** No custom fields: what tokens are named with until the workspace's are read. */
const NO_FIELDS: readonly FieldName[] = [];

/**
 * A campaign's sequence as an email sequence builder: on the left the timeline of its steps with
 * the waits between them, on the right the composer of the selected step. The two panes fill the
 * screen and scroll on their own; on a phone they stack. Save sends the whole ordered list to
 * `campaigns.update` (existing steps and variants keep their ids), and the API's errors show
 * beside the inputs they name, with a dot on the steps and variants that hold them. Once the page
 * has loaded, what the person opens eases in (`moving`); the first render appears without motion.
 */
export const SequenceEditor = () => {
  const campaign = useCampaign();
  const workspace = useWorkspace();
  const editor = useCampaignEditor(
    campaign,
    fromCampaign,
    sequenceChanges,
    "Sequence saved"
  );
  const fields = useQuery(fieldsQuery(workspace)).data ?? NO_FIELDS;
  const steps = editor.draft;
  const readOnly = !campaignEditable(workspace, campaign);
  const errors = fieldProblems(editor.failure);
  const [stepSelection, setStepSelection] = useState<Selection>({
    index: 0,
    key: steps[0]?.key ?? "",
  });
  const [variantSelection, setVariantSelection] = useState<Selection>({
    index: 0,
    key: "",
  });
  const [view, setView] = useState<View>("edit");
  const [person, setPerson] = useState<PersonObject | null>(null);
  const [moving, setMoving] = useState(false);
  // The step just added: it grows into the timeline and its body takes the cursor.
  const [added, setAdded] = useState<string | null>(null);
  const composer = useRef<HTMLElement>(null);

  const stepIndex = resolve(
    steps.map((step) => step.key),
    stepSelection
  );
  const step = steps[stepIndex];
  const variantIndex = step
    ? resolve(
        step.variants.map((variant) => variant.key),
        variantSelection
      )
    : -1;
  const variant = step?.variants[variantIndex];

  // A list that changes shape moves the paths the last errors pointed at: drop them.
  const reshape = (next: StepDraft[]) => {
    editor.setDraft(next);
    editor.setFailure(null);
  };
  const update = (key: string, patch: Partial<StepDraft>) =>
    editor.setDraft((list) =>
      list.map((item) => (item.key === key ? { ...item, ...patch } : item))
    );
  const select = (index: number) => {
    const chosen = steps[index];
    if (!chosen) {
      return;
    }
    setStepSelection({ index, key: chosen.key });
    setVariantSelection({ index: 0, key: "" });
    setMoving(true);
    if (chosen.key !== added) {
      setAdded(null);
    }
  };
  /** Adds a step at `index` (the end by default), opens it and puts the cursor in its body. */
  const add = (at?: number) => {
    const index = at ?? steps.length;
    const created = newStep(index + 1, steps[index - 1]);
    reshape(steps.toSpliced(index, 0, created));
    setStepSelection({ index, key: created.key });
    setVariantSelection({ index: 0, key: "" });
    setAdded(created.key);
    setView("edit");
    setMoving(true);
  };
  const showComposer = () => {
    if (!window.matchMedia(SIDE_BY_SIDE).matches) {
      composer.current?.scrollIntoView({
        behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
          ? "auto"
          : "smooth",
        block: "start",
      });
    }
  };

  const saveBar = (divided: boolean) => (
    <SaveBar
      dirty={editor.dirty}
      divided={divided}
      label="Save sequence"
      note={
        campaign.status === "draft"
          ? undefined
          : "Saved changes reach only the emails not yet created."
      }
      onDiscard={() => editor.reset(editor.base)}
      onSave={() => {
        void editor.save();
      }}
      saving={editor.saving}
    />
  );
  const problems = (
    <>
      <ArchivedNote campaign={campaign} />
      <SaveProblem
        failure={editor.failure}
        onReload={editor.handleReload}
        unplaced={unplaced(errors)}
      />
    </>
  );

  if (!step || !variant) {
    return (
      <div className="flex flex-col gap-4">
        {editor.dirty && !readOnly ? (
          <div className="flex justify-end">{saveBar(false)}</div>
        ) : null}
        {problems}
        <EmptyPanel
          action={
            readOnly ? undefined : (
              <Button onClick={() => add()} variant="primary">
                <HugeiconsIcon icon={Add01Icon} />
                Write the first email
              </Button>
            )
          }
          description="The first email goes out when someone is enrolled; each follow-up waits for the time you set after the one before it."
          icon={Mail01Icon}
          illustration="campaign"
          title="No emails yet"
        />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4 lg:h-(--fit-height)" ref={fitToPanel}>
      {problems}
      <div className="flex min-h-0 flex-1 flex-col gap-8 lg:flex-row lg:gap-0">
        <div className="lg:border-line min-h-0 shrink-0 scrollbar-thin lg:w-[300px] lg:overflow-y-auto lg:border-r lg:pr-5">
          <Timeline
            added={added}
            errors={errors}
            fields={fields}
            onAdd={() => add()}
            onAddAfter={(index) => add(index + 1)}
            onChange={update}
            onDelete={(key) =>
              reshape(steps.filter((item) => item.key !== key))
            }
            onMove={(from, to) => {
              reshape(moved(steps, from, to));
              setMoving(true);
            }}
            onSelect={(index) => {
              select(index);
              showComposer();
            }}
            readOnly={readOnly}
            selected={stepIndex}
            steps={steps}
          />
        </div>
        <section
          aria-label={`Step ${stepIndex + 1}: ${step.name}`}
          className="min-h-0 min-w-0 flex-1 scroll-mt-4 scrollbar-thin lg:overflow-y-auto lg:pr-2 lg:pl-6"
          ref={composer}
        >
          <div className="max-w-[880px]">
            <Composer
              autoFocus={added === step.key}
              base={editor.base}
              campaign={campaign}
              canTest={canWrite(workspace) && campaign.status !== "archived"}
              errors={errors}
              fields={fields}
              index={stepIndex}
              key={step.key}
              moving={moving}
              onChange={(patch) => update(step.key, patch)}
              onPerson={setPerson}
              onSelectVariant={(key) => {
                setVariantSelection({
                  index: Math.max(
                    0,
                    step.variants.findIndex((item) => item.key === key)
                  ),
                  key,
                });
                setMoving(true);
              }}
              onStep={select}
              onView={(next) => {
                setView(next);
                setMoving(true);
              }}
              person={person}
              readOnly={readOnly}
              save={readOnly ? undefined : saveBar(true)}
              step={step}
              total={steps.length}
              variant={variant}
              view={view}
            />
          </div>
        </section>
      </div>
    </div>
  );
};
