/**
 * The illustration library: single-line drawings on a 240 × 160 canvas, kept as data so the same
 * drawing becomes an SVG file (`toSvg`) or React elements, with no markup injected anywhere.
 *
 * The rules, so a new drawing belongs to the set: grey lines 2 units wide with round caps and
 * joins; a ground line where the object stands; no fills except the one pink element; exactly one
 * pink element, placed where the action is (a reply, the next step, the flag that says "mail to
 * send"). Colours are CSS variables with the dark theme's values as fallbacks, so an inlined
 * drawing follows the page's theme and a drawing used as an image still reads on black.
 *
 * Lines that draw themselves have the class `ln` and `pathLength` 1; the dotted path is `dots`;
 * the pink element `dot` and its pulse `ping` (see motion/norbelys-motion.css).
 */

/** One SVG element: its tag and its attributes, named the way React names them. */
export type Shape = readonly [
  "path" | "circle",
  Readonly<Record<string, string | number>>,
];

/** A drawing: a file name, what it shows (its accessible label) and its shapes. */
export interface Illustration {
  readonly name: string;
  readonly label: string;
  readonly shapes: readonly Shape[];
}

const LINE = "var(--nb-illus-line, #8a8a8a)";
const FAINT = "var(--nb-illus-faint, #3a3a3a)";
const GROUND = "var(--nb-illus-ground, #262626)";
const GRID_LINE = "var(--nb-illus-grid, #161616)";
const PINK = "var(--nb-illus-accent, #f472b6)";
const ON_PINK = "var(--nb-illus-on-accent, #000000)";

/** A line that draws itself. */
const ln = (d: string, stroke = LINE): Shape => [
  "path",
  { className: "ln", d, pathLength: 1, stroke },
];
/** The dotted pink path: the way a message travels. */
const dots = (d: string): Shape => [
  "path",
  { className: "dots", d, stroke: PINK, strokeDasharray: "0.5 7" },
];
/** The pink point and its pulse. */
const point = (cx: number, cy: number): Shape[] => [
  ["circle", { className: "dot", cx, cy, fill: PINK, r: 4.5, stroke: "none" }],
  ["circle", { className: "ping", cx, cy, r: 10, stroke: PINK }],
];
/** A pink shape that appears with the point (a flag, a badge). */
const pinkShape = (
  d: string,
  extra: Record<string, string | number> = {}
): Shape => [
  "path",
  { className: "dot", d, fill: PINK, stroke: PINK, ...extra },
];

/** Every drawing, with the state it illustrates. */
export const ILLUSTRATIONS = [
  {
    label: "Your mailbox sends, and a reply comes back",
    name: "welcome",
    shapes: [
      ln("M16 136H224", GROUND),
      ln("M44 136V108"),
      ln("M22 108V92a14 14 0 0 1 14-14h16a14 14 0 0 1 14 14v16Z"),
      ln("M66 96V70h12l-3 4 3 4H66", FAINT),
      ln("M70 92c10 0 18-6 26-8", FAINT),
      ln(
        "M98 76a4 4 0 0 1 4-4h40a4 4 0 0 1 4 4v24a4 4 0 0 1-4 4h-40a4 4 0 0 1-4-4Z"
      ),
      ln("M100 75l22 15 22-15"),
      dots("M148 82c12-2 16-12 24-16"),
      ln(
        "M180 46h32a8 8 0 0 1 8 8v12a8 8 0 0 1-8 8h-20l-8 7v-7h-4a8 8 0 0 1-8-8V54a8 8 0 0 1 8-8Z"
      ),
      ln("M184 56h24M184 64h14", FAINT),
      ...point(220, 46),
    ],
  },
  {
    label: "An envelope sending its first message",
    name: "first-send",
    shapes: [
      ln("M20 134H220", GROUND),
      ln(
        "M42 60a8 8 0 0 1 8-8h80a8 8 0 0 1 8 8v48a8 8 0 0 1-8 8H50a8 8 0 0 1-8-8Z"
      ),
      ln("M46 58l44 30 44-30"),
      dots("M146 84c24 0 30-36 52-38"),
      ...point(204, 46),
    ],
  },
  {
    label: "A mailbox with its flag up: mail to send",
    name: "mailbox",
    shapes: [
      ln("M20 140H220", GROUND),
      ln("M118 140V104"),
      ln("M66 104V76a24 24 0 0 1 24-24h56a24 24 0 0 1 24 24v28Z"),
      ln("M50 72h26v18H50z"),
      ln("M51 73l12 8 12-8"),
      ln("M170 92V40", PINK),
      pinkShape("M170 40h24l-5 7 5 7h-24Z"),
    ],
  },
  {
    label: "A report on its way",
    name: "reports",
    shapes: [
      ln("M36 128H212", GROUND),
      ln("M36 100H212M36 72H212M36 44H212", GRID_LINE),
      ln("M36 116c18-4 26-20 44-20s24 10 40 4 22-30 42-34"),
      dots("M162 66c14-3 22-14 34-18"),
      ...point(200, 46),
    ],
  },
  {
    label: "An inbox with a reply waiting",
    name: "inbox",
    shapes: [
      ln("M20 140H220", GROUND),
      ln("M64 100l12-34h88l12 34"),
      ln("M52 100h34l8 14h52l8-14h34v24a6 6 0 0 1-6 6H58a6 6 0 0 1-6-6Z"),
      ln(
        "M100 30h40a8 8 0 0 1 8 8v12a8 8 0 0 1-8 8h-22l-10 8v-8h-8a8 8 0 0 1-8-8V38a8 8 0 0 1 8-8Z"
      ),
      ln("M108 44h24", FAINT),
      ...point(150, 30),
    ],
  },
  {
    label: "People, ready to be reached",
    name: "people",
    shapes: [
      ln("M20 140H220", GROUND),
      ln("M86 36h96a6 6 0 0 1 6 6v54a6 6 0 0 1-6 6", FAINT),
      ln(
        "M58 52h108a6 6 0 0 1 6 6v62a6 6 0 0 1-6 6H58a6 6 0 0 1-6-6V58a6 6 0 0 1 6-6Z"
      ),
      ln("M76 89a10 10 0 1 0 20 0a10 10 0 1 0-20 0"),
      ln("M70 112c4-8 10-12 16-12s12 4 16 12"),
      ln("M114 80h40M114 92h26", FAINT),
      ...point(166, 52),
    ],
  },
  {
    label: "An event travelling to an endpoint",
    name: "webhook",
    shapes: [
      ln("M20 140H220", GROUND),
      ln(
        "M40 70a6 6 0 0 1 6-6h28a6 6 0 0 1 6 6v28a6 6 0 0 1-6 6H46a6 6 0 0 1-6-6Z"
      ),
      ln("M52 78h16M52 90h10", FAINT),
      ln("M164 84a20 20 0 1 0 40 0a20 20 0 1 0-40 0"),
      ln("M178 78l-6 6 6 6M190 78l6 6-6 6", FAINT),
      dots("M84 84c22 0 26-20 42-20s22 20 36 20"),
      ...point(126, 64),
    ],
  },
  {
    label: "An API key",
    name: "key",
    shapes: [
      ln("M20 140H220", GROUND),
      ln("M64 84a20 20 0 1 0 40 0a20 20 0 1 0-40 0"),
      ln("M104 84h76l8 8M160 84v12M172 84v9"),
      ln("M78 84a6 6 0 1 0 12 0a6 6 0 1 0-12 0", FAINT),
      ...point(84, 84),
    ],
  },
  {
    label: "A sending domain, verified",
    name: "domain",
    shapes: [
      ln("M20 140H220", GROUND),
      ln("M86 84a34 34 0 1 0 68 0a34 34 0 1 0-68 0"),
      ln("M106 84a14 34 0 1 0 28 0a14 34 0 1 0-28 0"),
      ln("M86 84h68M92 66h56M92 102h56", FAINT),
      [
        "circle",
        {
          className: "dot",
          cx: 152,
          cy: 56,
          fill: PINK,
          r: 11,
          stroke: "none",
        },
      ],
      [
        "path",
        {
          className: "dot",
          d: "M147 56l3.5 3.5 6.5-7",
          stroke: ON_PINK,
          strokeWidth: 2.4,
        },
      ],
    ],
  },
  {
    label: "A campaign: steps in a sequence",
    name: "campaign",
    shapes: [
      ln("M20 134H220", GROUND),
      ln(
        "M34 70a4 4 0 0 1 4-4h32a4 4 0 0 1 4 4v24a4 4 0 0 1-4 4H38a4 4 0 0 1-4-4Z"
      ),
      ln("M36 69l18 13 18-13"),
      ln(
        "M100 70a4 4 0 0 1 4-4h32a4 4 0 0 1 4 4v24a4 4 0 0 1-4 4h-32a4 4 0 0 1-4-4Z"
      ),
      ln("M102 69l18 13 18-13"),
      ln("M74 82h26", FAINT),
      dots("M140 82h26"),
      ln(
        "M166 70a4 4 0 0 1 4-4h32a4 4 0 0 1 4 4v24a4 4 0 0 1-4 4h-32a4 4 0 0 1-4-4Z",
        PINK
      ),
      ln("M168 69l18 13 18-13", PINK),
      ln("M54 112v6M120 112v6M186 112v6", FAINT),
    ],
  },
  {
    label: "A message that found no one",
    name: "not-found",
    shapes: [
      ln("M20 134H220", GROUND),
      ln(
        "M52 64a6 6 0 0 1 6-6h60a6 6 0 0 1 6 6v36a6 6 0 0 1-6 6H58a6 6 0 0 1-6-6Z"
      ),
      ln("M55 62l33 22 33-22"),
      dots("M126 82c30 0 50-8 50-26s-20-20-28-10"),
      ...point(146, 50),
    ],
  },
] as const satisfies readonly Illustration[];

/** The name of a drawing in the library. */
export type IllustrationName = (typeof ILLUSTRATIONS)[number]["name"];

/** SVG's names for the attributes React names in camel case. */
const SVG_NAMES: Record<string, string> = {
  className: "class",
  strokeDasharray: "stroke-dasharray",
  strokeWidth: "stroke-width",
};

/** The drawing as a standalone SVG document. */
export const toSvg = ({ label, shapes }: Illustration) => {
  const body = shapes
    .map(([tag, attributes]) => {
      const pairs = Object.entries(attributes).map(
        ([key, value]) => `${SVG_NAMES[key] ?? key}="${value}"`
      );
      return `<${tag} ${pairs.join(" ")}/>`;
    })
    .join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 160" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" role="img" aria-label="${label}"><title>${label}</title>${body}</svg>`;
};
