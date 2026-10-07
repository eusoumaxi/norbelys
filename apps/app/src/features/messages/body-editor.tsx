import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { useImperativeHandle, useMemo, useRef, useState } from "react";
import type { ReactNode, Ref } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/segmented";
import { Textarea } from "@/components/ui/textarea";
import type { FieldName } from "@/features/messages/merge-tags";
import { TemplateEditor } from "@/features/messages/template-editor";
import type { TemplateEditorHandle } from "@/features/messages/template-editor";
import { escapeHtml, templateParts } from "@/features/messages/templates";
import { fieldsQuery } from "@/features/people/queries";
import { useWorkspace } from "@/lib/workspace";

type BodyFormat = "text" | "html";

/**
 * A message body being written: the format in use, the text Write mode shows, and `html`, what
 * the API receives. In Write mode `html` is the text converted (`textToHtml`); in HTML mode it is
 * the markup as typed. Switching converts, so what is shown is always what is sent.
 */
export interface BodyDraft {
  format: BodyFormat;
  text: string;
  html: string;
}

export const EMPTY_BODY: BodyDraft = { format: "text", html: "", text: "" };

/**
 * A web address in Write mode's text: `http(s)://` and what follows up to a space, a quote or an
 * angle bracket. It may hold template tags (`?name={{ person.given_name }}`).
 */
const ADDRESS = /https?:\/\/(?:\{\{.*?\}\}|[^\s<>"'{}])+/gu;

/** A link Write mode made: its address shown as its own words. */
const OWN_LINK =
  /<a href="(?<href>(?:\{\{.*?\}\}|[^\s<>"'{}])+?)">\k<href><\/a>/gu;

/** `text` with `change` applied to everything but its template tags, which stay as written. */
const outsideTags = (text: string, change: (plain: string) => string) =>
  templateParts(text)
    .map((part) => (part.tag ? part.text : change(part.text)))
    .join("");

const unescapeText = (text: string): string =>
  text.replaceAll("&lt;", "<").replaceAll("&gt;", ">").replaceAll("&amp;", "&");

/** Punctuation that ends a sentence rather than an address: "see https://norbelys.com." */
const SENTENCE_END = new Set([".", ",", ";", ":", "!", "?"]);

const count = (text: string, character: string) =>
  text.split(character).length - 1;

/** An address without the punctuation of the sentence around it, or a bracket it did not open. */
const trimAddress = (address: string): string => {
  let end = address.length;
  while (end > 0) {
    const last = address.charAt(end - 1);
    const kept = address.slice(0, end);
    const unopened = last === ")" && count(kept, ")") > count(kept, "(");
    if (!SENTENCE_END.has(last) && !unopened) {
      break;
    }
    end -= 1;
  }
  return address.slice(0, end);
};

/** One paragraph of Write mode's text as HTML: escaped, its addresses linked, its breaks kept. */
const paragraphHtml = (text: string): string => {
  const tags = templateParts(text).filter((part) => part.tag);
  const inTag = (index: number) =>
    tags.some((tag) => index > tag.from && index < tag.from + tag.text.length);
  let html = "";
  let from = 0;
  for (const match of text.matchAll(ADDRESS)) {
    const address = trimAddress(match[0]);
    if (!inTag(match.index) && /\/\/./u.test(address)) {
      const href = outsideTags(address, escapeHtml);
      html += `${outsideTags(text.slice(from, match.index), escapeHtml)}<a href="${href}">${href}</a>`;
      from = match.index + address.length;
    }
  }
  html += outsideTags(text.slice(from), escapeHtml);
  return html.replaceAll("\n", "<br>\n");
};

/**
 * Write mode's text as the HTML body the API takes: a paragraph per block between blank lines,
 * line breaks kept, web addresses turned into links, markup characters escaped. Template tags
 * (`{{ person.given_name }}`) are left as they are, so they work in Write mode too.
 */
const textToHtml = (text: string): string =>
  text
    .replaceAll("\r\n", "\n")
    .trim()
    .split(/\n\s*\n/u)
    .filter((block) => block.trim() !== "")
    .map((block) => `<p>${paragraphHtml(block.trim())}</p>`)
    .join("\n");

/**
 * `html` with the spacing between its paragraphs and after its line breaks written the way Write
 * mode writes it: HTML from elsewhere (the API, another tool) often joins `</p><p>` or writes
 * `<br/>`, which a reader never sees, and which must not push a plain email into HTML mode.
 */
const tidy = (html: string): string =>
  html
    .trim()
    .replaceAll(/<\/p>\s*<p>/gu, "</p>\n<p>")
    .replaceAll(/<br\s*\/?>\s*/gu, "<br>\n");

/**
 * The text Write mode shows for `html`, when `html` is what Write mode makes of it (but for the
 * spacing `tidy` evens out), so it saves back as the same email; `null` for any other HTML.
 */
const htmlToText = (html: string): string | null => {
  if (html === "") {
    return "";
  }
  const tidied = tidy(html);
  if (!tidied.startsWith("<p>") || !tidied.endsWith("</p>")) {
    return null;
  }
  const text = tidied
    .slice("<p>".length, -"</p>".length)
    .split("</p>\n<p>")
    .map((block) =>
      outsideTags(
        block.replaceAll("<br>\n", "\n").replaceAll(OWN_LINK, "$<href>"),
        unescapeText
      )
    )
    .join("\n\n");
  return textToHtml(text) === tidied ? text : null;
};

/**
 * A saved HTML body as a draft: in Write mode when the HTML is exactly what Write mode makes, so
 * it saves back unchanged; otherwise in HTML mode, as written.
 */
export const bodyDraftOf = (html: string): BodyDraft => {
  const text = htmlToText(html);
  return text === null
    ? { format: "html", html, text: "" }
    : { format: "text", html, text };
};

/** The HTML the API receives for a draft: the HTML as written, or the text converted. */
export const bodyHtml = (draft: BodyDraft): string => {
  if (draft.format === "html") {
    return draft.html.trim();
  }
  const written = textToHtml(draft.text);
  // Opened and not retyped: the saved HTML as it was, so only spacing never counts as a change.
  return tidy(draft.html) === written ? draft.html : written;
};

/** Elements whose content no reader sees. */
const UNSEEN = new Set(["HEAD", "SCRIPT", "STYLE", "TEMPLATE", "TITLE"]);

/** Elements that make a paragraph of their own. */
const BLOCKS = new Set([
  "ADDRESS",
  "ARTICLE",
  "ASIDE",
  "BLOCKQUOTE",
  "DD",
  "DIV",
  "DL",
  "DT",
  "FIGURE",
  "FOOTER",
  "H1",
  "H2",
  "H3",
  "H4",
  "H5",
  "H6",
  "HEADER",
  "HR",
  "LI",
  "MAIN",
  "OL",
  "P",
  "PRE",
  "SECTION",
  "TABLE",
  "TR",
  "UL",
]);

/**
 * The words of any HTML body as Write mode's text: blocks as paragraphs, `<br>` as line breaks, a
 * link as its words followed by its address when they differ. Formatting, images and styles are
 * dropped (the switch asks first); template tags are text, so they stay.
 */
const htmlWords = (html: string): string => {
  const parsed = new DOMParser().parseFromString(html, "text/html");
  const words: string[] = [];
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      words.push((node.textContent ?? "").replaceAll(/\s+/gu, " "));
      return;
    }
    if (!(node instanceof Element) || UNSEEN.has(node.tagName)) {
      return;
    }
    if (node.tagName === "BR") {
      words.push("\n");
      return;
    }
    const block = BLOCKS.has(node.tagName);
    words.push(block ? "\n\n" : "");
    for (const child of node.childNodes) {
      walk(child);
    }
    const href = node.tagName === "A" ? node.getAttribute("href") : null;
    if (href && /^https?:/u.test(href) && node.textContent?.trim() !== href) {
      words.push(` (${href})`);
    }
    words.push(block ? "\n\n" : "");
  };
  walk(parsed.body);
  return words
    .join("")
    .split("\n")
    .map((line) => line.trim())
    .join("\n")
    .replaceAll(/\n{3,}/gu, "\n\n")
    .trim();
};

/**
 * The page a mail client would show: the body on a white canvas with the message's own colors
 * (not the dashboard's), links opening in a new tab. The `nb-` classes mark what a preview filled
 * in: a value the person lacks, a snippet the AI writes when it sends, a sample field.
 */
const mailDocument = (html: string): string =>
  `<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; font-src 'none'; form-action 'none'; base-uri 'none'"><meta name="viewport" content="width=device-width"><base target="_blank"><style>html{background:#fff}body{margin:20px 24px;font:14px/1.6 system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;color:#0a0a0a;overflow-wrap:anywhere}p{margin:0 0 14px}img{max-width:100%;height:auto}.nb-missing,.nb-ai,.nb-sample{border-radius:3px;padding:0 4px;font-size:13px}.nb-missing{background:#fde8e6;color:#b42318}.nb-ai{border:1px dashed #a1a1aa;color:#52525b}.nb-sample{background:#f4f4f5;color:#3f3f46}</style>${html}`;

/**
 * Parse in an inert template before entering the frame. Remove navigation and embedded
 * documents; the frame's CSP separately blocks remote resources and its sandbox blocks scripts.
 * The retained source is never changed by this display-only projection.
 */
const passiveMail = (html: string): string => {
  const template = document.createElement("template");
  template.innerHTML = html;
  for (const element of template.content.querySelectorAll(
    "script, meta, base, link, iframe, frame, object, embed, form, template"
  )) {
    element.remove();
  }
  for (const element of template.content.querySelectorAll("*")) {
    for (const attribute of [...element.attributes]) {
      if (
        [
          "href",
          "xlink:href",
          "action",
          "formaction",
          "ping",
          "autofocus",
        ].includes(attribute.name) ||
        attribute.name.startsWith("on")
      ) {
        element.removeAttribute(attribute.name);
      }
    }
  }
  return template.innerHTML;
};

/**
 * An email body as a mail client shows it, in a frame that runs no script (its document may be
 * read, to grow the frame to its content).
 */
export const MailFrame = ({
  className,
  html,
  readOnly = false,
  title,
}: {
  className?: string;
  html: string;
  readOnly?: boolean;
  title: string;
}) => {
  const [height, setHeight] = useState<number>();
  const documentHtml = useMemo(
    () => mailDocument(readOnly ? passiveMail(html) : html),
    [html, readOnly]
  );
  return (
    <iframe
      className={cn("border-line min-h-40 w-full rounded-sm border", className)}
      onLoad={(event) =>
        setHeight(
          event.currentTarget.contentDocument?.documentElement.scrollHeight
        )
      }
      sandbox="allow-same-origin"
      referrerPolicy="no-referrer"
      srcDoc={documentHtml}
      style={height === undefined ? undefined : { height }}
      title={title}
    />
  );
};

const FORMATS = [
  { label: "Write", value: "text" as const },
  { label: "HTML", value: "html" as const },
];

/** No custom fields: what the editor names tokens with until the workspace's are read. */
const NO_FIELDS: readonly FieldName[] = [];

/**
 * How the editor sits in its page: `field`, a bordered control among a form's fields; `sheet`, the
 * body of an email under its subject, with no field chrome of its own. `area` and `code` are the
 * box and type of the writing area in Write and HTML mode.
 */
const LOOKS = {
  field: {
    area: "border-field-line bg-field hover:border-line-strong focus-visible:border-focus aria-invalid:border-error-line min-h-56 rounded-sm border px-3 py-2 text-sm transition-colors",
    code: "min-h-56 font-mono text-xs",
    notice: "rounded-sm border border-line px-3 py-2",
    root: "flex flex-col gap-2",
    toolbar: "flex flex-wrap items-center gap-2",
  },
  sheet: {
    area: "caret-accent min-h-72 px-5 py-4 text-base leading-6",
    code: "caret-accent min-h-72 resize-none rounded-none border-transparent bg-transparent px-5 py-4 font-mono text-xs leading-5 hover:border-transparent focus-visible:border-transparent",
    notice: "border-line border-b px-5 py-2",
    root: "flex flex-col",
    toolbar: "border-line flex flex-wrap items-center gap-2 border-b px-3 py-2",
  },
};

/** What a parent can ask of a body editor. */
export interface BodyEditorHandle {
  focus: () => void;
  /** Puts a template tag where the cursor last was: as a token in Write mode, as code in HTML. */
  insert: (tag: string) => void;
}

/**
 * Where a message's body is written. The API takes one HTML body (it derives the plain-text
 * alternative itself): Write mode is text with its template tags as tokens, converted to
 * paragraphs and links on the way; HTML mode sends the markup as written. Switching to Write
 * keeps HTML that Write mode made; other HTML loses its formatting, so the switch asks first.
 * Markup pasted in Write mode is offered as the HTML body rather than typed out as text. The
 * preview renders the body in a sandboxed frame that runs no script.
 */
export const BodyEditor = ({
  draft,
  id,
  invalid,
  label = "Body",
  onChange,
  placeholder,
  previewable = true,
  readOnly = false,
  ref,
  toolbar,
  variant = "field",
}: {
  draft: BodyDraft;
  id: string;
  invalid: boolean;
  /** The accessible name of the writing area. */
  label?: string;
  onChange: (draft: BodyDraft) => void;
  placeholder?: string;
  /** Whether the editor offers its own preview; an editor inside a larger preview has none. */
  previewable?: boolean;
  readOnly?: boolean;
  ref?: Ref<BodyEditorHandle>;
  /** Controls beside the format switch, such as a menu of template tags. */
  toolbar?: ReactNode;
  variant?: keyof typeof LOOKS;
}) => {
  const workspace = useWorkspace();
  const fields = useQuery(fieldsQuery(workspace)).data ?? NO_FIELDS;
  const [previewing, setPreviewing] = useState(false);
  const [confirming, setConfirming] = useState(false);
  // Markup pasted in Write mode, while the person decides what it is.
  const [pasted, setPasted] = useState<string | null>(null);
  const template = useRef<TemplateEditorHandle>(null);
  const code = useRef<HTMLTextAreaElement>(null);
  const look = LOOKS[variant];
  const html = draft.format === "html";
  const writeWords = (text: string) =>
    onChange({ format: "text", html: textToHtml(text), text });
  const switchTo = (format: BodyFormat) => {
    if (format === draft.format) {
      return;
    }
    if (format === "html") {
      onChange({ ...draft, format, html: textToHtml(draft.text) });
      return;
    }
    const exact = htmlToText(draft.html) ?? htmlToText(draft.html.trim());
    if (exact !== null) {
      writeWords(exact);
    } else if (draft.html.trim() === "") {
      writeWords("");
    } else {
      setConfirming(true);
    }
  };

  useImperativeHandle(ref, () => ({
    focus: () => (html ? code.current?.focus() : template.current?.focus()),
    insert: (tag) => {
      if (!html) {
        template.current?.insert(tag);
        return;
      }
      const area = code.current;
      const start = area?.selectionStart ?? draft.html.length;
      const end = area?.selectionEnd ?? start;
      onChange({
        ...draft,
        html: draft.html.slice(0, start) + tag + draft.html.slice(end),
      });
      requestAnimationFrame(() => {
        area?.focus();
        area?.setSelectionRange(start + tag.length, start + tag.length);
      });
    },
  }));

  const showToolbar = !readOnly || toolbar !== undefined || previewable;
  return (
    <div className={look.root}>
      {showToolbar ? (
        <div className={look.toolbar}>
          {readOnly ? null : (
            <Segmented<BodyFormat>
              label="Body format"
              onChange={switchTo}
              options={FORMATS}
              value={draft.format}
            />
          )}
          {toolbar}
          {previewable ? (
            <Button
              className="ml-auto"
              onClick={() => setPreviewing((value) => !value)}
              size="s"
              variant="tertiary"
            >
              {previewing ? "Edit" : "Preview"}
            </Button>
          ) : null}
        </div>
      ) : null}
      {pasted === null ? null : (
        <div
          className={cn(
            "flex flex-wrap items-center gap-x-3 gap-y-2 text-sm",
            look.notice
          )}
          aria-live="polite"
        >
          <span className="text-fg-2 mr-auto">
            That looks like HTML. Use it as this email&apos;s HTML (it replaces
            what is written), or paste it as text?
          </span>
          <Button
            onClick={() => {
              onChange({ format: "html", html: pasted, text: "" });
              setPasted(null);
            }}
            size="s"
          >
            Use as HTML
          </Button>
          <Button
            onClick={() => {
              template.current?.insert(pasted);
              setPasted(null);
            }}
            size="s"
            variant="tertiary"
          >
            Paste as text
          </Button>
        </div>
      )}
      {previewing ? (
        <MailFrame html={bodyHtml(draft)} title="Body preview" />
      ) : null}
      {!previewing && html ? (
        <Textarea
          aria-invalid={invalid}
          aria-label={label}
          className={look.code}
          id={id}
          onChange={(event) => onChange({ ...draft, html: event.target.value })}
          placeholder="<p>Hi {{ person.given_name }},</p>"
          readOnly={readOnly}
          ref={code}
          spellCheck={false}
          value={draft.html}
        />
      ) : null}
      {previewing || html ? null : (
        <TemplateEditor
          className={look.area}
          fields={fields}
          id={id}
          invalid={invalid}
          label={label}
          multiline
          onChange={writeWords}
          onPasteHtml={setPasted}
          placeholder={placeholder}
          readOnly={readOnly}
          ref={template}
          value={draft.text}
        />
      )}
      <ConfirmDialog
        confirmLabel="Remove formatting"
        danger
        description="Write mode keeps paragraphs, line breaks and links. The rest of this HTML (formatting, images, styles) is removed from the body."
        onConfirm={() => writeWords(htmlWords(draft.html))}
        onOpenChange={setConfirming}
        open={confirming}
        title="Switch to Write?"
      />
    </div>
  );
};
