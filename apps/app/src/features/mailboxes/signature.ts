import { escapeHtml } from "@/features/messages/templates";

/**
 * A sender's signature, written once.
 *
 * The API keeps two parts on each sender (`signature_html` for the HTML part of its mail,
 * `signature_text` for the plain-text part) and derives the missing one when a message is
 * rendered: HTML from the text (escaped, its line breaks kept), text from the HTML (its plain
 * words). So a person writes one signature, as it should look, and the dashboard saves it as
 * text with no HTML: the two parts of an email can then never disagree.
 *
 * A developer may still set HTML through the API (a logo, links, a layout). The dashboard never
 * drops it silently: it shows that signature as it renders, and replaces it only when the person
 * chooses to write a new one.
 */

/** A sender's two signature parts, as an identity carries them. */
interface SignatureParts {
  signature_html?: string | null;
  signature_text?: string | null;
}

const filled = (value: string | null | undefined): value is string =>
  typeof value === "string" && value.trim() !== "";

/**
 * The text as it is saved: Windows line breaks made plain, blank lines before it and spaces
 * after it removed (spaces inside, such as the `-- ` that opens a signature, are kept), and
 * `null` when it is blank, so an emptied signature is cleared rather than saved as spaces.
 */
export const cleanSignature = (text: string): string | null => {
  const clean = text
    .replaceAll("\r\n", "\n")
    .replace(/^(?:[ \t]*\n)+/u, "")
    .trimEnd();
  return clean === "" ? null : clean;
};

/**
 * The HTML a written signature becomes in the HTML part of an email, as the server derives it:
 * escaped, each line break kept as one; `null` for a blank one.
 */
export const writtenHtml = (text: string): string | null => {
  const clean = cleanSignature(text);
  return clean === null ? null : escapeHtml(clean).replaceAll("\n", "<br>");
};

/** The HTML signature set through the API, when the sender has one. */
export const apiHtml = (sender: SignatureParts): string | null =>
  filled(sender.signature_html) ? sender.signature_html : null;

/**
 * The HTML that ends a sender's emails: the HTML set through the API, else the written text made
 * HTML; `null` when the sender has no signature. What a preview of its mail shows.
 */
export const signatureHtml = (sender: SignatureParts): string | null =>
  apiHtml(sender) ?? writtenHtml(sender.signature_text ?? "");

/** Elements whose content a mail client never shows. */
const UNSEEN = new Set(["HEAD", "SCRIPT", "STYLE", "TEMPLATE", "TITLE"]);

/**
 * Elements that start a line of their own. Table cells are among them: a signature laid out in
 * a table (a name beside a link) reads as one line per cell.
 */
const BLOCKS = new Set([
  "ADDRESS",
  "BLOCKQUOTE",
  "DD",
  "DIV",
  "DT",
  "H1",
  "H2",
  "H3",
  "H4",
  "H5",
  "H6",
  "HR",
  "LI",
  "OL",
  "P",
  "PRE",
  "SECTION",
  "TABLE",
  "TD",
  "TH",
  "TR",
  "UL",
]);

/**
 * The words of an HTML signature, one line per line it shows: blocks and cells end a line, and
 * the empty lines between them are dropped (a signature is a few short lines, not paragraphs).
 * The HTML is parsed into an inert document (no script runs and nothing loads) and only its
 * text is read.
 */
const htmlText = (html: string): string => {
  const parsed = new DOMParser().parseFromString(html, "text/html");
  const parts: string[] = [];
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      parts.push((node.textContent ?? "").replaceAll(/\s+/gu, " "));
      return;
    }
    if (!(node instanceof Element) || UNSEEN.has(node.tagName)) {
      return;
    }
    if (node.tagName === "BR") {
      parts.push("\n");
      return;
    }
    const block = BLOCKS.has(node.tagName);
    parts.push(block ? "\n" : "");
    for (const child of node.childNodes) {
      walk(child);
    }
    parts.push(block ? "\n" : "");
  };
  walk(parsed.body);
  return parts
    .join("")
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .join("\n");
};

/**
 * What a person starts from when writing a sender's signature: its text, or, when only HTML was
 * set, the words of that HTML.
 */
export const startingText = (sender: SignatureParts): string => {
  if (filled(sender.signature_text)) {
    return sender.signature_text;
  }
  const html = apiHtml(sender);
  return html === null ? "" : htmlText(html);
};

/** The signature as the lines a list shows: the words of its HTML, else its text; `""` for none. */
export const signatureLines = (sender: SignatureParts): string => {
  const html = apiHtml(sender);
  return html === null
    ? (cleanSignature(sender.signature_text ?? "") ?? "")
    : htmlText(html);
};
