/**
 * Where a template is written as text with its tags shown as tokens: `{{ person.given_name |
 * default("there") }}` reads "First name or “there”", an `{% if %}` reads "If Company is set".
 * Clicking a token changes what it prints for people who lack the value.
 *
 * The editor is a `contenteditable` element the browser edits natively, so typing, selection,
 * spell checking and undo behave as in any text field; the component only guards what a rich
 * editor would otherwise let in. A token is an element that cannot be edited inside and carries
 * its tag (`data-tag`); the text is read back from the elements as the template, tokens as their
 * tags. Line breaks are typed as `\n` (the element keeps white space), pasted text arrives as
 * text, formatting shortcuts do nothing, and the browser's own extra line break at the end (kept
 * so a last, empty line shows) is dropped when reading.
 *
 * The element's content is painted from `value` only when `value` is not what the element already
 * holds (another variant, a discard), never while the person types. Insertions go through the
 * browser's own editing commands, so they can be undone like typing. A tag typed by hand stays
 * text until the editor loses the focus, then turns into its token.
 */
import { Popover } from "@base-ui/react/popover";
import { AiMagicIcon, GitBranchIcon } from "@hugeicons/core-free-icons";
import type { IconSvgElement } from "@hugeicons/react";
import { cn } from "cn";
import {
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type { Ref } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { tagLook, withFallback } from "@/features/messages/merge-tags";
import type { FieldName, TagLook } from "@/features/messages/merge-tags";
import { escapeHtml, templateParts } from "@/features/messages/templates";

/** What a parent can ask of an editor. */
export interface TemplateEditorHandle {
  focus: () => void;
  /**
   * Puts `text` where the cursor last was (at the end before any), as if typed: its tags become
   * tokens, and the cursor goes after it.
   */
  insert: (text: string) => void;
}

/**
 * The look of a token, by kind: a value on a quiet ground, a snippet the AI writes outlined in
 * dashes, a condition outlined, template code in mono. Tokens never take the accent: they are
 * text to be filled in, not a call to act.
 */
export const TOKEN = {
  ai: "border border-dashed border-line-strong text-fg",
  base: "mx-px rounded-sm px-1 py-px whitespace-nowrap [box-decoration-break:clone]",
  code: "bg-hover font-mono text-xs text-fg-2",
  condition: "border border-line-strong text-fg-2",
  fallback: "text-fg-3",
  icon: "mr-1 inline-block size-3.5 align-[-2px] text-icon",
  value: "bg-selected text-fg",
} as const;

/** A token's hover text: what will be printed in its place, in words. */
const tokenTitle = (look: TagLook): string => {
  if (look.kind === "ai") {
    return `${look.label}: written by AI for each person; left out when it can't be. Click to change that.`;
  }
  if (look.kind === "condition") {
    return `${look.label}: the text after it shows only to the people it describes.`;
  }
  if (look.kind === "code") {
    return "Template code, used as written.";
  }
  const fallback =
    look.fallback === undefined
      ? ""
      : ` For people without one: ${look.fallback ? `“${look.fallback}”` : "nothing"}.`;
  return `Filled in with ${look.label.toLowerCase()} for each email.${fallback} Click to change that.`;
};

const SVG = "http://www.w3.org/2000/svg";

/** An icon of the set as an element, for a token built outside React. */
const iconElement = (icon: IconSvgElement): SVGSVGElement => {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("class", TOKEN.icon);
  for (const [name, attributes] of icon) {
    const shape = document.createElementNS(SVG, name);
    for (const [attribute, value] of Object.entries(attributes)) {
      if (attribute !== "key") {
        shape.setAttribute(
          attribute.replaceAll(
            /[A-Z]/gu,
            (letter) => `-${letter.toLowerCase()}`
          ),
          String(value)
        );
      }
    }
    svg.append(shape);
  }
  return svg;
};

/** The token of a tag: it cannot be edited inside, and it carries the tag it stands for. */
const tokenElement = (
  tag: string,
  fields: readonly FieldName[]
): HTMLElement => {
  const look = tagLook(tag, fields);
  const token = document.createElement("span");
  token.contentEditable = "false";
  token.dataset.tag = tag;
  token.className = cn(
    TOKEN.base,
    TOKEN[look.kind],
    look.kind === "value" || look.kind === "ai" ? "cursor-pointer" : null
  );
  token.title = tokenTitle(look);
  if (look.kind === "ai") {
    token.append(iconElement(AiMagicIcon));
  } else if (look.kind === "condition") {
    token.append(iconElement(GitBranchIcon));
  }
  token.append(look.label);
  if (look.fallback) {
    const fallback = document.createElement("span");
    fallback.className = TOKEN.fallback;
    fallback.textContent = ` or “${look.fallback}”`;
    token.append(fallback);
  }
  return token;
};

/** The nodes of a template: its text as text, each tag as its token. */
const templateNodes = (text: string, fields: readonly FieldName[]): Node[] =>
  templateParts(text).map((part) =>
    part.tag
      ? tokenElement(part.text, fields)
      : document.createTextNode(part.text)
  );

/**
 * The element holds `text`: its text and tokens, and one line break more when it ends with one,
 * so its last, empty line shows (as the browser itself keeps it while typing).
 */
const paint = (
  element: HTMLElement,
  text: string,
  fields: readonly FieldName[]
) => {
  const nodes = templateNodes(text, fields);
  if (text.endsWith("\n")) {
    nodes.push(document.createTextNode("\n"));
  }
  element.replaceChildren(...nodes);
};

/** Elements the browser may wrap a line in. */
const BLOCKS = new Set(["DIV", "LI", "P"]);

/** The template the nodes under `node` hold: text as written, tokens as their tags, lines broken. */
const readNodes = (node: Node): string => {
  let text = "";
  const walk = (parent: Node) => {
    for (const child of parent.childNodes) {
      if (child.nodeType === Node.TEXT_NODE) {
        text += (child.textContent ?? "")
          .replaceAll(" ", " ")
          .replaceAll("​", "");
      } else if (
        child instanceof HTMLElement &&
        child.dataset.tag !== undefined
      ) {
        text += child.dataset.tag;
      } else if (child instanceof HTMLBRElement) {
        text += "\n";
      } else if (child instanceof HTMLElement) {
        const block = BLOCKS.has(child.tagName);
        if (block && text !== "" && !text.endsWith("\n")) {
          text += "\n";
        }
        walk(child);
        if (block && !text.endsWith("\n")) {
          text += "\n";
        }
      }
    }
  };
  walk(node);
  return text;
};

/** The template an editor holds, without the line break the browser keeps at its end. */
const read = (element: HTMLElement): string => {
  const text = readNodes(element);
  return text.endsWith("\n") ? text.slice(0, -1) : text;
};

/** Whether text holds a tag written as text rather than as its token. */
const holdsTypedTag = (element: HTMLElement): boolean =>
  [...element.childNodes].some(
    (node) =>
      node.nodeType === Node.TEXT_NODE &&
      templateParts(node.textContent ?? "").some((part) => part.tag)
  );

/** Whether pasted text is markup rather than words: an element with its closing tag. */
const looksLikeHtml = (text: string): boolean =>
  /<(?<name>[a-z][a-z0-9]*)\b[^>]*>[\s\S]*<\/\k<name>\s*>/iu.test(text) ||
  /^\s*<(?:!doctype|html|table|div|p|br)\b/iu.test(text);

/** The marker an insertion ends with: where the cursor goes, removed once it is there. */
const CARET = '<span data-caret=""></span>';

/** Markup that inserts `text` as typed: escaped, its tags as tokens, its line breaks kept. */
const insertionMarkup = (text: string, fields: readonly FieldName[]): string =>
  templateNodes(text, fields)
    .map((node) =>
      node instanceof HTMLElement
        ? node.outerHTML
        : escapeHtml(node.textContent ?? "").replaceAll("\n", "<br>")
    )
    .join("") + CARET;

/** The cursor at the end of `element`. */
const endOf = (element: HTMLElement): Range => {
  const range = document.createRange();
  range.selectNodeContents(element);
  range.collapse(false);
  return range;
};

/** Puts the cursor right after `node`. */
const placeAfter = (node: Node) => {
  const selection = document.getSelection();
  const after = document.createRange();
  after.setStartAfter(node);
  after.collapse(true);
  selection?.removeAllRanges();
  selection?.addRange(after);
};

/** A token being changed: the element, the tag it carries and how it looks. */
interface Changing {
  element: HTMLElement;
  tag: string;
  look: TagLook;
}

/**
 * What a token prints for people who lack its value, changed in place: the fallback, or the
 * token removed. Opened by clicking the token.
 */
const FallbackPopover = ({
  changing,
  onClose,
  onRemove,
  onSet,
}: {
  changing: Changing | null;
  onClose: () => void;
  onRemove: () => void;
  onSet: (fallback: string) => void;
}) => {
  const [text, setText] = useState("");
  const [shown, setShown] = useState<Changing | null>(null);
  if (changing && changing !== shown) {
    setShown(changing);
    setText(changing.look.fallback ?? "");
  }
  const look = (changing ?? shown)?.look;
  return (
    <Popover.Root
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
      open={changing !== null}
    >
      <Popover.Portal>
        <Popover.Positioner
          align="start"
          anchor={(changing ?? shown)?.element ?? null}
          className="z-50 outline-none"
          sideOffset={6}
        >
          <Popover.Popup className="border-line bg-surface text-fg shadow-menu flex w-72 flex-col gap-3 rounded-sm border p-3 transition-[opacity,translate] duration-120 ease-(--nb-ease-out) outline-none data-ending-style:opacity-0 data-starting-style:-translate-y-1 data-starting-style:opacity-0 motion-reduce:transition-none">
            <form
              className="flex flex-col gap-3"
              onSubmit={(event) => {
                event.preventDefault();
                onSet(text);
              }}
            >
              <Popover.Title className="text-sm font-semibold">
                {look?.label}
              </Popover.Title>
              <label className="flex flex-col gap-1.5 text-xs">
                <span className="text-fg-2">
                  {look?.kind === "ai"
                    ? "When the AI can't write it, write instead:"
                    : "For people without one, write instead:"}
                </span>
                <Input
                  onChange={(event) => setText(event.target.value)}
                  placeholder="Nothing"
                  value={text}
                />
              </label>
              <div className="flex items-center justify-between gap-2">
                <Button onClick={onRemove} size="s" variant="tertiary">
                  Remove from the email
                </Button>
                <Button size="s" type="submit" variant="primary">
                  Done
                </Button>
              </div>
            </form>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
};

/**
 * A template written as text with tokens (see the module). `multiline` takes line breaks (a
 * body); without it, Enter does nothing and pasted lines join (a subject). Pasted markup goes to
 * `onPasteHtml` when it is given. `className` sets the box and the type, shared with the
 * placeholder drawn over the empty editor.
 */
export const TemplateEditor = ({
  className,
  fields,
  id,
  invalid = false,
  label,
  labelledBy,
  multiline = false,
  onChange,
  onPasteHtml,
  placeholder,
  readOnly = false,
  ref,
  value,
}: {
  className?: string;
  /** The workspace's custom fields, which name their tokens. */
  fields: readonly FieldName[];
  id: string;
  invalid?: boolean;
  /** The accessible name, when no element names the editor (`labelledBy`). */
  label?: string;
  labelledBy?: string;
  multiline?: boolean;
  onChange: (value: string) => void;
  onPasteHtml?: (html: string) => void;
  placeholder?: string;
  readOnly?: boolean;
  ref?: Ref<TemplateEditorHandle>;
  value: string;
}) => {
  const element = useRef<HTMLDivElement>(null);
  // What the element holds, as last painted or read, and with which field names.
  const held = useRef<string | null>(null);
  const named = useRef("");
  // Where the cursor last was in the editor, for an insertion made from a menu.
  const cursor = useRef<Range | null>(null);
  const [changing, setChanging] = useState<Changing | null>(null);
  const names = fields.map((field) => `${field.key}:${field.label}`).join("|");

  useLayoutEffect(() => {
    const editor = element.current;
    if (editor && (value !== held.current || names !== named.current)) {
      paint(editor, value, fields);
      held.current = value;
      named.current = names;
    }
  });

  useEffect(() => {
    const remember = () => {
      const selection = document.getSelection();
      const editor = element.current;
      if (
        editor &&
        selection &&
        selection.rangeCount > 0 &&
        editor.contains(selection.anchorNode)
      ) {
        cursor.current = selection.getRangeAt(0).cloneRange();
      }
    };
    document.addEventListener("selectionchange", remember);
    return () => document.removeEventListener("selectionchange", remember);
  }, []);

  /** Reads what the person wrote and hands it on. */
  const changed = () => {
    const editor = element.current;
    if (!editor) {
      return;
    }
    const text = read(editor);
    held.current = text;
    onChange(multiline ? text : text.replaceAll("\n", " "));
  };

  /** Puts `text` where the cursor last was, the cursor after it, as typing would. */
  const insert = (text: string) => {
    const editor = element.current;
    const selection = document.getSelection();
    if (!editor || !selection || readOnly) {
      return;
    }
    editor.focus();
    const at = cursor.current;
    selection.removeAllRanges();
    selection.addRange(
      at && editor.contains(at.startContainer) ? at : endOf(editor)
    );
    // The browser's own insertion keeps undo working; the marker at its end takes the cursor.
    document.execCommand(
      "insertHTML",
      false,
      insertionMarkup(
        multiline ? text : text.replaceAll(/\s*\n\s*/gu, " "),
        fields
      )
    );
    const marker = editor.querySelector("[data-caret]");
    if (marker) {
      const before = document.createRange();
      before.setStartBefore(marker);
      before.collapse(true);
      marker.remove();
      selection.removeAllRanges();
      selection.addRange(before);
    }
  };

  useImperativeHandle(ref, () => ({
    focus: () => element.current?.focus(),
    insert,
  }));

  /** The template under the selection, tokens as their tags, for the clipboard. */
  const selected = (): string | null => {
    const selection = document.getSelection();
    const editor = element.current;
    if (!editor || !selection || selection.rangeCount === 0) {
      return null;
    }
    const range = selection.getRangeAt(0);
    if (range.collapsed || !editor.contains(range.commonAncestorContainer)) {
      return null;
    }
    const holder = document.createElement("div");
    holder.append(range.cloneContents());
    return readNodes(holder);
  };

  /** Replaces the token being changed with `node` (or removes it), and reads the result. */
  const replaceToken = (node: Node | null) => {
    if (!changing) {
      return;
    }
    const { element: token } = changing;
    if (node) {
      token.replaceWith(node);
    } else {
      token.remove();
    }
    setChanging(null);
    changed();
    element.current?.focus();
    if (node) {
      placeAfter(node);
    }
  };

  return (
    <div className="relative min-w-0 flex-1">
      {value === "" && placeholder ? (
        <div
          aria-hidden
          className={cn(
            "text-fg-3 pointer-events-none absolute inset-0 overflow-hidden",
            className
          )}
        >
          {placeholder}
        </div>
      ) : null}
      <div
        aria-invalid={invalid}
        aria-label={labelledBy ? undefined : label}
        aria-labelledby={labelledBy}
        aria-multiline={multiline}
        aria-placeholder={placeholder}
        aria-readonly={readOnly}
        className={cn(
          "relative break-words whitespace-pre-wrap outline-none",
          className
        )}
        contentEditable={!readOnly}
        id={id}
        onBlur={() => {
          // A tag typed by hand turns into its token once the person moves on.
          const editor = element.current;
          if (editor && holdsTypedTag(editor)) {
            paint(editor, held.current ?? value, fields);
          }
        }}
        onClick={(event) => {
          const target =
            event.target instanceof Element
              ? event.target.closest<HTMLElement>("[data-tag]")
              : null;
          const tag = target?.dataset.tag;
          if (!target || tag === undefined || readOnly) {
            return;
          }
          const look = tagLook(tag, fields);
          if (look.kind === "value" || look.kind === "ai") {
            setChanging({ element: target, look, tag });
          }
        }}
        onCompositionEnd={changed}
        onCopy={(event) => {
          const text = selected();
          if (text !== null) {
            event.preventDefault();
            event.clipboardData.setData("text/plain", text);
          }
        }}
        onCut={(event) => {
          const text = selected();
          if (text !== null) {
            event.preventDefault();
            event.clipboardData.setData("text/plain", text);
            document.execCommand("delete");
          }
        }}
        onDrop={(event) => event.preventDefault()}
        onInput={changed}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.nativeEvent.isComposing) {
            event.preventDefault();
            if (multiline) {
              document.execCommand("insertLineBreak");
            }
          } else if (
            (event.metaKey || event.ctrlKey) &&
            ["b", "i", "u"].includes(event.key.toLowerCase())
          ) {
            // Write mode sends text: no bold, italics or underline sneak in.
            event.preventDefault();
          }
        }}
        onPaste={(event) => {
          event.preventDefault();
          const text = event.clipboardData.getData("text/plain");
          if (multiline && onPasteHtml && looksLikeHtml(text)) {
            onPasteHtml(text);
          } else {
            insert(text);
          }
        }}
        ref={element}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a textarea cannot hold tokens; a contenteditable element announces itself as a text field through this role
        role="textbox"
        spellCheck
        suppressContentEditableWarning
        tabIndex={readOnly ? -1 : 0}
      />
      <FallbackPopover
        changing={changing}
        onClose={() => setChanging(null)}
        onRemove={() => replaceToken(null)}
        onSet={(fallback) => {
          const tag = changing ? withFallback(changing.tag, fallback) : null;
          if (tag === null) {
            setChanging(null);
          } else {
            replaceToken(tokenElement(tag, fields));
          }
        }}
      />
    </div>
  );
};
