import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

import { FieldError } from "@/components/ui/field";
import { snippetNames } from "@/features/campaigns/sequence/draft";
import type {
  StepDraft,
  VariantDraft,
} from "@/features/campaigns/sequence/draft";
import { PersonalizeMenu } from "@/features/campaigns/sequence/personalize-menu";
import { BodyEditor } from "@/features/messages/body-editor";
import type { BodyEditorHandle } from "@/features/messages/body-editor";
import { snippetTags } from "@/features/messages/merge-tags";
import type { FieldName } from "@/features/messages/merge-tags";
import { TemplateEditor } from "@/features/messages/template-editor";
import type { TemplateEditorHandle } from "@/features/messages/template-editor";

/** One line of the email's head: its name on the left, the field, the problem under it. */
const HeadRow = ({
  children,
  id,
  label,
  problem,
}: {
  children: ReactNode;
  /** The id of the name, which names the field. */
  id: string;
  label: string;
  problem?: string;
}) => (
  <div className="border-line border-b">
    <div className="flex min-h-11 items-center gap-x-2 pr-2 pl-5 max-sm:flex-wrap max-sm:pt-2.5">
      {/* On a phone the name takes a line of its own: the field keeps the width to be read. */}
      <span
        className="text-fg-3 w-24 shrink-0 text-sm max-sm:w-full max-sm:text-xs"
        id={id}
      >
        {label}
      </span>
      {children}
    </div>
    {problem ? <FieldError className="px-5 pb-2">{problem}</FieldError> : null}
  </div>
);

/** The box and type of a line of the head: one line of the email, read first. */
const headLine = "caret-accent py-3 text-base";

/**
 * One variant written as an email: its subject, its preview text (hidden until asked for or
 * set) and its body, each personalised from the menu beside it, details showing as tokens. The
 * API's errors for the variant (`steps[i].variants[j].subject`, `.preheader`, `.html`) show
 * under their fields.
 */
export const EmailEditor = ({
  at,
  autoFocus,
  errors,
  fields,
  onChange,
  onEnableAi,
  readOnly,
  step,
  variant,
}: {
  /** The variant's path in the saved list, as the API names fields: `steps[0].variants[1]`. */
  at: string;
  /** Puts the cursor in the body once mounted: a follow-up just added is written at once. */
  autoFocus: boolean;
  errors: Readonly<Record<string, string>>;
  fields: readonly FieldName[];
  onChange: (patch: Partial<VariantDraft>) => void;
  onEnableAi: () => void;
  readOnly: boolean;
  step: StepDraft;
  variant: VariantDraft;
}) => {
  const subject = useRef<TemplateEditorHandle>(null);
  const preheader = useRef<TemplateEditorHandle>(null);
  const body = useRef<BodyEditorHandle>(null);
  const [preheaderShown, setPreheaderShown] = useState(
    variant.preheader !== ""
  );
  const snippets = step.personalised ? snippetTags(snippetNames(step)) : null;
  const id = (field: string) => `${variant.key}-${field}`;
  const menu = { fields, onEnableAi, snippets };

  useEffect(() => {
    if (autoFocus) {
      body.current?.focus();
    }
  }, [autoFocus]);

  return (
    <div className="border-line has-focus-visible:border-line-strong rounded-lg border transition-colors">
      <HeadRow
        id={id("subject-label")}
        label="Subject"
        problem={errors[`${at}.subject`]}
      >
        <TemplateEditor
          className={`${headLine} font-medium`}
          fields={fields}
          id={id("subject")}
          invalid={Boolean(errors[`${at}.subject`])}
          labelledBy={id("subject-label")}
          onChange={(next) => onChange({ subject: next })}
          placeholder="Quick question"
          readOnly={readOnly}
          ref={subject}
          value={variant.subject}
        />
        {readOnly || preheaderShown ? null : (
          <button
            className="text-fg-3 hover:text-fg focus-visible:outline-focus flex h-7 shrink-0 cursor-pointer items-center gap-1 rounded-sm px-2 text-xs font-semibold outline-none focus-visible:outline-1"
            onClick={() => {
              setPreheaderShown(true);
              requestAnimationFrame(() => preheader.current?.focus());
            }}
            type="button"
          >
            <HugeiconsIcon className="size-3.5" icon={Add01Icon} />
            Preview text
          </button>
        )}
        {readOnly ? null : (
          <PersonalizeMenu
            {...menu}
            compact
            label="Personalize the subject"
            onInsert={(text) => subject.current?.insert(text)}
          />
        )}
      </HeadRow>
      {preheaderShown ? (
        <HeadRow
          id={id("preheader-label")}
          label="Preview text"
          problem={errors[`${at}.preheader`]}
        >
          <TemplateEditor
            className={headLine}
            fields={fields}
            id={id("preheader")}
            invalid={Boolean(errors[`${at}.preheader`])}
            labelledBy={id("preheader-label")}
            onChange={(next) => onChange({ preheader: next })}
            placeholder="The line inboxes show after the subject"
            readOnly={readOnly}
            ref={preheader}
            value={variant.preheader}
          />
          {readOnly ? null : (
            <PersonalizeMenu
              {...menu}
              compact
              label="Personalize the preview text"
              onInsert={(text) => preheader.current?.insert(text)}
            />
          )}
        </HeadRow>
      ) : null}
      <BodyEditor
        draft={variant.body}
        id={id("body")}
        invalid={Boolean(errors[`${at}.html`])}
        label="Email body"
        onChange={(next) => onChange({ body: next })}
        placeholder="Write your email as you would in your mailbox. A blank line starts a new paragraph; links work as you type them."
        previewable={false}
        readOnly={readOnly}
        ref={body}
        toolbar={
          readOnly ? undefined : (
            <PersonalizeMenu
              {...menu}
              label="Personalize the body"
              onInsert={(text) => body.current?.insert(text)}
            />
          )
        }
        variant="sheet"
      />
      {errors[`${at}.html`] ? (
        <FieldError className="border-line border-t px-5 py-2">
          {errors[`${at}.html`]}
        </FieldError>
      ) : null}
    </div>
  );
};
