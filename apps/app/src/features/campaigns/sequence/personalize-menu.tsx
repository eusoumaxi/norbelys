import { Popover } from "@base-ui/react/popover";
import {
  AiMagicIcon,
  GitBranchIcon,
  HelpCircleIcon,
  UserEdit01Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import {
  conditionTemplate,
  fieldTag,
  PERSON_TAGS,
  SENDER_TAGS,
  tagFor,
} from "@/features/messages/merge-tags";
import type { FieldName, MergeTag, Test } from "@/features/messages/merge-tags";
import { TemplateEditor } from "@/features/messages/template-editor";
import type { TemplateEditorHandle } from "@/features/messages/template-editor";

/** The look of the menu's popovers: a panel over the page, fading in from its trigger. */
const panelClasses =
  "border-line bg-surface text-fg shadow-menu flex flex-col gap-3 rounded-sm border p-4 transition-[opacity,translate] duration-120 ease-(--nb-ease-out) outline-none data-ending-style:opacity-0 data-starting-style:-translate-y-1 data-starting-style:opacity-0 motion-reduce:transition-none";

/** One group of the menu: what it holds in words, then a row per detail with its fallback. */
const TagGroup = ({
  description,
  label,
  onInsert,
  tags,
}: {
  description: string;
  label: string;
  onInsert: (tag: string) => void;
  tags: readonly MergeTag[];
}) => (
  <DropdownMenuGroup>
    <DropdownMenuLabel className="h-auto flex-col items-start gap-0 pt-2.5 pb-1">
      <span>{label}</span>
      <span className="text-fg-3 text-xs font-normal">{description}</span>
    </DropdownMenuLabel>
    {tags.map((tag) => (
      <DropdownMenuItem key={tag.path} onClick={() => onInsert(tag.tag)}>
        <span className="min-w-0 flex-1 truncate">{tag.label}</span>
        {tag.fallback ? (
          <span className="text-fg-3 max-w-36 truncate text-xs font-normal">
            or “{tag.fallback}”
          </span>
        ) : null}
      </DropdownMenuItem>
    ))}
  </DropdownMenuGroup>
);

/** A popover of the menu, placed under its trigger. */
const Panel = ({
  anchor,
  children,
  className,
  finalFocus,
  onClose,
  onSettled,
  open,
}: {
  anchor: RefObject<HTMLElement | null>;
  children: ReactNode;
  className: string;
  /** Whether closing gives the focus back to the menu's trigger. */
  finalFocus: () => boolean;
  onClose: () => void;
  /** Called once the panel has finished closing (or opening). */
  onSettled?: (open: boolean) => void;
  open: boolean;
}) => (
  <Popover.Root
    onOpenChange={(next) => {
      if (!next) {
        onClose();
      }
    }}
    onOpenChangeComplete={onSettled}
    open={open}
  >
    <Popover.Portal>
      <Popover.Positioner
        align="start"
        anchor={anchor}
        className="z-50 outline-none"
        collisionPadding={16}
        sideOffset={6}
      >
        <Popover.Popup
          className={`${panelClasses} ${className}`}
          finalFocus={finalFocus}
        >
          {children}
        </Popover.Popup>
      </Popover.Positioner>
    </Popover.Portal>
  </Popover.Root>
);

const TESTS: { label: string; value: Test }[] = [
  { label: "is known", value: "set" },
  { label: "is unknown", value: "empty" },
  { label: "is", value: "is" },
  { label: "isn't", value: "isNot" },
];

const isTest = (value: string): value is Test =>
  TESTS.some((test) => test.value === value);

/**
 * "Show text to some people only", without template code: the detail to look at, what it must
 * be, what those people read, and what everyone else reads. Inserts the `if` it describes, which
 * the editor then shows as tokens in the same words.
 */
const ConditionPanel = ({
  anchor,
  fields,
  finalFocus,
  onClose,
  onInsert,
  onSettled,
  open,
}: {
  anchor: RefObject<HTMLElement | null>;
  fields: readonly FieldName[];
  finalFocus: () => boolean;
  onClose: () => void;
  onInsert: (text: string) => void;
  onSettled: (open: boolean) => void;
  open: boolean;
}) => {
  const [path, setPath] = useState("person.company");
  const [test, setTest] = useState<Test>("set");
  const [value, setValue] = useState("");
  const [shown, setShown] = useState("");
  const [otherwise, setOtherwise] = useState("");
  const sentence = useRef<TemplateEditorHandle>(null);
  const details = [
    ...PERSON_TAGS.filter((tag) => tag.path !== "person.email"),
    ...fields.map(fieldTag),
  ];
  const name = details.find((tag) => tag.path === path)?.label ?? "";
  const compared = test === "is" || test === "isNot";
  const ready = shown.trim() !== "" && (!compared || value.trim() !== "");
  return (
    <Panel
      anchor={anchor}
      className="w-96"
      finalFocus={finalFocus}
      onClose={onClose}
      onSettled={onSettled}
      open={open}
    >
      <div className="flex flex-col gap-1">
        <Popover.Title className="text-sm font-semibold">
          Show text to some people only
        </Popover.Title>
        <Popover.Description className="text-fg-3 text-xs">
          For example, mention the company only to the people whose company you
          know, and write something else for everyone else.
        </Popover.Description>
      </div>
      <form
        className="flex flex-col gap-3"
        onSubmit={(event) => {
          event.preventDefault();
          if (ready) {
            onInsert(
              conditionTemplate({ otherwise, path, shown, test, value })
            );
            setShown("");
            setOtherwise("");
          }
        }}
      >
        <div className="flex flex-col gap-1.5">
          <span className="text-fg-2 text-xs font-semibold" id="condition-when">
            When their
          </span>
          <div className="flex flex-wrap items-center gap-2">
            <Select
              className="w-36"
              label="Detail"
              onChange={setPath}
              options={details.map((tag) => ({
                label: tag.label,
                value: tag.path,
              }))}
              value={path}
            />
            <Select
              className="w-32"
              label="Test"
              onChange={(next) => {
                if (isTest(next)) {
                  setTest(next);
                }
              }}
              options={TESTS}
              value={test}
            />
            {compared ? (
              <Input
                aria-label="Value"
                className="min-w-24 flex-1"
                onChange={(event) => setValue(event.target.value)}
                placeholder="Healthcare"
                value={value}
              />
            ) : null}
          </div>
        </div>
        <div className="flex flex-col gap-1.5">
          <span className="text-fg-2 text-xs font-semibold" id="condition-then">
            They read
          </span>
          <div className="border-field-line bg-field focus-within:border-focus flex items-center rounded-sm border pr-1 transition-colors">
            <TemplateEditor
              className="min-h-8 px-3 py-1.5 text-sm"
              fields={fields}
              id="condition-then-text"
              labelledBy="condition-then"
              onChange={setShown}
              placeholder="The sentence they read"
              ref={sentence}
              value={shown}
            />
            {test === "empty" ? null : (
              <Button
                onClick={() => sentence.current?.insert(tagFor(path))}
                size="s"
                variant="tertiary"
              >
                Add {name.toLowerCase()}
              </Button>
            )}
          </div>
        </div>
        <div className="flex flex-col gap-1.5">
          <span className="text-fg-2 text-xs font-semibold" id="condition-else">
            Everyone else reads
          </span>
          <div className="border-field-line bg-field focus-within:border-focus rounded-sm border transition-colors">
            <TemplateEditor
              className="min-h-8 px-3 py-1.5 text-sm"
              fields={fields}
              id="condition-else-text"
              labelledBy="condition-else"
              onChange={setOtherwise}
              placeholder="Nothing"
              value={otherwise}
            />
          </div>
        </div>
        <div className="flex justify-end gap-2">
          <Popover.Close render={<Button size="s" variant="tertiary" />}>
            Cancel
          </Popover.Close>
          <Button disabled={!ready} size="s" type="submit" variant="primary">
            Insert
          </Button>
        </div>
      </form>
    </Panel>
  );
};

/** How personalising works, in a salesperson's words; template code comes last, for developers. */
const HelpPanel = ({
  anchor,
  onClose,
  open,
}: {
  anchor: RefObject<HTMLElement | null>;
  onClose: () => void;
  open: boolean;
}) => (
  <Panel
    anchor={anchor}
    className="w-96"
    finalFocus={() => true}
    onClose={onClose}
    open={open}
  >
    <Popover.Title className="text-sm font-semibold">
      Personalizing an email
    </Popover.Title>
    <dl className="flex flex-col gap-3 text-xs">
      <div className="flex flex-col gap-0.5">
        <dt className="text-fg font-semibold">Insert a detail</dt>
        <dd className="text-fg-2">
          Pick First name from Personalize: Ada reads “Hi Ada,” and someone
          without a first name reads the word after “or”, “Hi there,”. Click a
          detail in the email to change that word.
        </dd>
      </div>
      <div className="flex flex-col gap-0.5">
        <dt className="text-fg font-semibold">Show text to some people only</dt>
        <dd className="text-fg-2">
          Mention the company only to the people whose company you know, and
          write something else for everyone else.
        </dd>
      </div>
      <div className="flex flex-col gap-0.5">
        <dt className="text-fg font-semibold">Let AI write a line</dt>
        <dd className="text-fg-2">
          Turn on Personalize with AI in the step&apos;s settings and insert an
          AI line: Norbe writes it for each person from your instructions.
        </dd>
      </div>
      <div className="flex flex-col gap-0.5">
        <dt className="text-fg font-semibold">Check it</dt>
        <dd className="text-fg-2">
          Preview shows the email as one of your people will read it; Send a
          test delivers it to your inbox.
        </dd>
      </div>
    </dl>
    <p className="text-fg-3 border-line border-t pt-3 text-xs">
      For developers: details are MiniJinja tags you can also type, such as{" "}
      <code className="font-mono">{"{{ person.company }}"}</code>.
    </p>
  </Panel>
);

/**
 * Puts a person's details where the cursor is in a subject or a body: the person's own details,
 * the workspace's custom fields, the sender's, and the AI lines of a step personalised with AI
 * (or, when it is not, the way to turn that on); a sentence shown to some people only; and how
 * all this works, in plain words. Each detail a person may lack comes with a fallback.
 */
export const PersonalizeMenu = ({
  compact = false,
  fields,
  label,
  onEnableAi,
  onInsert,
  snippets,
}: {
  /** An icon alone, for a one-line field. */
  compact?: boolean;
  fields: readonly FieldName[];
  /** The trigger's accessible name: what it personalises. */
  label: string;
  onEnableAi: () => void;
  /** Puts text (a tag, an `if` with its texts) where the cursor is, and the focus there. */
  onInsert: (text: string) => void;
  /** The AI lines to offer; `null` while the step is not personalised with AI. */
  snippets: readonly MergeTag[] | null;
}) => {
  const trigger = useRef<HTMLButtonElement>(null);
  // What was chosen: inserted once the menu or panel has closed, so the focus it gives back
  // cannot pull the cursor out of the text.
  const pending = useRef<string | null>(null);
  const [panel, setPanel] = useState<"condition" | "help" | null>(null);
  const insert = (text: string) => {
    pending.current = text;
  };
  /** Whether closing gives the focus back to the trigger: not when the text takes it. */
  const giveBack = () => pending.current === null;
  /** Once closed, puts what was chosen in the text (which takes the focus). */
  const settle = (open: boolean) => {
    const text = pending.current;
    if (!open && text !== null) {
      pending.current = null;
      // After the closing overlay has given the focus back, so the text keeps it.
      requestAnimationFrame(() => onInsert(text));
    }
  };
  return (
    <>
      <DropdownMenu onOpenChangeComplete={settle}>
        <DropdownMenuTrigger
          aria-label={label}
          ref={trigger}
          render={<Button size={compact ? "icon-s" : "s"} variant="tertiary" />}
        >
          <HugeiconsIcon icon={UserEdit01Icon} />
          {compact ? null : "Personalize"}
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align={compact ? "end" : "start"}
          className="w-80"
          finalFocus={giveBack}
        >
          <TagGroup
            description="Their details, from People"
            label="Person"
            onInsert={insert}
            tags={PERSON_TAGS}
          />
          {fields.length > 0 ? (
            <TagGroup
              description="The fields you added to People"
              label="Custom fields"
              onInsert={insert}
              tags={fields.map(fieldTag)}
            />
          ) : null}
          <TagGroup
            description="The mailbox the email is sent from"
            label="Sender"
            onInsert={insert}
            tags={SENDER_TAGS}
          />
          {snippets ? (
            <TagGroup
              description="A line Norbe writes for each person"
              label="Written by AI"
              onInsert={insert}
              tags={snippets}
            />
          ) : null}
          <DropdownMenuGroup>
            <DropdownMenuItem onClick={() => setPanel("condition")}>
              <HugeiconsIcon icon={GitBranchIcon} />
              Show text to some people only…
            </DropdownMenuItem>
            {snippets ? null : (
              <DropdownMenuItem onClick={onEnableAi}>
                <HugeiconsIcon icon={AiMagicIcon} />
                Let AI write a line…
              </DropdownMenuItem>
            )}
            <DropdownMenuItem onClick={() => setPanel("help")}>
              <HugeiconsIcon icon={HelpCircleIcon} />
              How personalizing works
            </DropdownMenuItem>
          </DropdownMenuGroup>
        </DropdownMenuContent>
      </DropdownMenu>
      <ConditionPanel
        anchor={trigger}
        fields={fields}
        finalFocus={giveBack}
        onClose={() => setPanel(null)}
        onInsert={(text) => {
          insert(text);
          setPanel(null);
        }}
        onSettled={settle}
        open={panel === "condition"}
      />
      <HelpPanel
        anchor={trigger}
        onClose={() => setPanel(null)}
        open={panel === "help"}
      />
    </>
  );
};
