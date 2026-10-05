import type { satteri } from "@astrojs/markdown-satteri";

import { COMPANY } from "./company";

/**
 * Lays out a legal document's Markdown as clauses: everything from one `##` heading to the next
 * becomes a `<section class="clause">`, and a quote written straight under the heading becomes
 * the clause's plain-words note (`role="note"`), set beside the clause on a wide screen.
 *
 * A heading's leading number ("4. Your data", "4.1 Exports") is set apart for styling, and its
 * anchor comes from its words alone (`#your-data`), so renumbering a document never breaks a link
 * someone saved. Every table is wrapped so it can scroll, and each cell carries its column's name
 * for phones, where rows stack. A placeholder such as `{{address}}` is filled in from the
 * company's facts (src/company.ts), so a detail changes in one place. Only documents in
 * `src/content/legal` are touched; the blog renders as before.
 */
type Entry = NonNullable<
  NonNullable<Parameters<typeof satteri>[0]>["hastPlugins"]
>[number];
type Factory = Extract<Entry, (context: never) => unknown>;
// A factory is a function, and functions have a `name` too: leave it out of the definition.
type Definition = Exclude<Extract<Entry, { name: string }>, Factory>;
type Hook = NonNullable<Definition["before"]>;
type Root = Parameters<Hook>[0];
type Node = Root["children"][number];
type Element = Extract<Node, { type: "element" }>;
type Content = Element["children"][number];

const NUMBER = /^(?<number>\d+(?:\.\d+)*)\.?\s+/u;

const element = (
  tagName: string,
  className: string,
  children: Content[],
  extra: Record<string, string> = {}
): Element => ({
  children,
  properties: { className: [className], ...extra },
  tagName,
  type: "element",
});

const isElement = (node: Node | Content, tagName: string): node is Element =>
  node.type === "element" && node.tagName === tagName;

const isBlank = (node: Node | Content): boolean =>
  node.type === "text" && node.value.trim() === "";

const textOf = (node: Node | Content): string => {
  if (node.type === "text") {
    return node.value;
  }
  return "children" in node ? node.children.map(textOf).join("") : "";
};

const slugOf = (text: string): string =>
  text
    .toLowerCase()
    .normalize("NFKD")
    .replaceAll(/\p{M}/gu, "")
    .replaceAll(/[^a-z0-9\s-]/gu, "")
    .trim()
    .replaceAll(/[\s-]+/gu, "-");

/** The company facts a document may name through a placeholder. */
const FACTS = new Map<string, string>([
  ["address", COMPANY.address.join(", ")],
  ["payments", COMPANY.payments],
  ["paymentsCompany", COMPANY.paymentsCompany],
  ["regions", COMPANY.regions],
]);

/** A text with its `{{placeholders}}` filled in; one the company doesn't have stops the build. */
const filled = (text: string): string =>
  text.replaceAll(/\{\{[a-zA-Z]+\}\}/gu, (token) => {
    const fact = FACTS.get(token.slice(2, -2));
    if (fact === undefined) {
      throw new Error(`${token} names no company fact (src/company.ts)`);
    }
    return fact;
  });

/** Fills the placeholders in a node and in everything inside it. */
const fill = (node: Content): Content => {
  if (node.type === "text") {
    return { ...node, value: filled(node.value) };
  }
  if (node.type === "element") {
    return { ...node, children: node.children.map(fill) };
  }
  return node;
};

/** A heading with its number set apart and an anchor made from its words, unique in the page. */
const numbered = (heading: Element, used: Set<string>): Element => {
  const [first, ...rest] = heading.children;
  const base = slugOf(textOf(heading).replace(NUMBER, "")) || "section";
  let id = base;
  for (let count = 2; used.has(id); count += 1) {
    id = `${base}-${count}`;
  }
  used.add(id);
  const match = first?.type === "text" ? NUMBER.exec(first.value) : null;
  const children: Content[] =
    first?.type === "text" && match
      ? [
          element("span", "clause-number", [
            { type: "text", value: match.groups?.number ?? "" },
          ]),
          { type: "text", value: ` ${first.value.slice(match[0].length)}` },
          ...rest,
        ]
      : heading.children;
  return { ...heading, children, properties: { ...heading.properties, id } };
};

/** A table that scrolls on its own, with each cell labelled by its column for stacked rows. */
const labelled = (table: Element): Element => {
  const head = table.children.find((child) => isElement(child, "thead"));
  const names = (head?.type === "element" ? head.children : [])
    .filter((row) => isElement(row, "tr"))
    .flatMap((row) =>
      row.type === "element"
        ? row.children.filter((cell) => cell.type === "element").map(textOf)
        : []
    );
  const label = (row: Content): Content => {
    if (!isElement(row, "tr")) {
      return row;
    }
    const cells = row.children.filter((cell) => cell.type === "element");
    return {
      ...row,
      children: cells.map((cell, index) =>
        isElement(cell, "td")
          ? {
              ...cell,
              properties: { ...cell.properties, dataLabel: names[index] ?? "" },
            }
          : cell
      ),
    };
  };
  const children = table.children.map((group) =>
    isElement(group, "tbody")
      ? { ...group, children: group.children.map(label) }
      : group
  );
  return element("div", "legal-table", [{ ...table, children }]);
};

/** What may move into a clause: everything but the nodes that only a document's root can hold. */
type Placeable = Exclude<Node, { type: "doctype" | "mdxjsEsm" }>;
const placeable = (node: Node): node is Placeable =>
  node.type !== "doctype" && node.type !== "mdxjsEsm";

/** The document's top level, regrouped into an introduction and its clauses. */
const arrange = (nodes: readonly Node[]): Node[] => {
  const used = new Set<string>();
  const arranged: Node[] = [];
  let intro: Element | undefined;
  let clause: Element | undefined;
  let body: Element | undefined;
  for (const node of nodes) {
    if (!placeable(node) || isBlank(node)) {
      continue;
    }
    if (node.type === "element" && node.tagName === "h2") {
      body = element("div", "clause-body", []);
      clause = element("section", "clause", [numbered(node, used), body]);
      arranged.push(clause);
      continue;
    }
    let placed: Content = node;
    if (node.type === "element" && node.tagName === "h3") {
      placed = numbered(node, used);
    } else if (node.type === "element" && node.tagName === "table") {
      placed = labelled(node);
    }
    placed = fill(placed);
    if (!clause || !body) {
      if (!intro) {
        intro = element("div", "clause-intro", []);
        arranged.push(intro);
      }
      intro.children.push(placed);
    } else if (
      node.type === "element" &&
      node.tagName === "blockquote" &&
      body.children.length === 0 &&
      clause.children.length === 2
    ) {
      clause.children.splice(
        1,
        0,
        element(
          "div",
          "clause-note",
          [
            element("p", "clause-note-label", [
              { type: "text", value: "In plain words" },
            ]),
            ...node.children.filter((child) => !isBlank(child)).map(fill),
          ],
          { role: "note" }
        )
      );
    } else {
      body.children.push(placed);
    }
  }
  return arranged;
};

const definition: Definition = {
  before(root, context) {
    context.replaceNode(root, {
      children: arrange(root.children),
      type: "root",
    });
  },
  name: "legal-clauses",
};

/** The plugin, for `markdown.processor`: it sits out every document but the legal ones. */
export const clauses: Factory = ({ fileURL }) =>
  fileURL?.pathname.includes("/content/legal/") ? definition : false;
