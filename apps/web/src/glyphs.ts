/**
 * The site's own icons, drawn on the mark's 64-unit grid with round caps and joins.
 *
 * Product icons are an ink line plus one pink point placed where the action is (the message
 * leaving the envelope, the reply landing in the tray, the needle's reading), the way the
 * brand's illustrations keep exactly one pink element. The point is what moves on hover.
 * Interface glyphs (arrows, plus, check) are the same line without the point.
 */
/** One icon: its lines, and its pink points with their radius. */
export interface Drawing {
  readonly lines: readonly string[];
  readonly points?: readonly (readonly [number, number])[];
  readonly radius?: number;
}

export const GLYPHS = {
  // Products and capabilities.
  ai: {
    lines: [
      "M41 46H24L11 56L13 40.5A15 15 0 0 1 7.5 31V23A15 15 0 0 1 22.5 8H41.5A15 15 0 0 1 56.5 23V34",
    ],
    points: [
      [24, 27],
      [32, 27],
      [40, 27],
    ],
    radius: 3.6,
  },
  broadcasts: {
    lines: [
      "M30 19L44 13V51L18 39H12A4 4 0 0 1 8 35V29A4 4 0 0 1 12 25H18L24 22",
    ],
    points: [[54, 32]],
  },
  campaigns: {
    lines: [
      "M50 20L32 33L14 20",
      "M41 52H19.5A12 12 0 0 1 7.5 40V24A12 12 0 0 1 19.5 12H44.5A12 12 0 0 1 56.5 24V37",
    ],
    points: [[51, 48]],
  },
  connect: {
    lines: [
      "M26 7V16",
      "M38 7V16",
      "M18 21V16H46V28A14 14 0 0 1 18 28V26",
      "M32 42V48",
    ],
    points: [[32, 55]],
  },
  inbox: {
    lines: [
      "M13 8L24 22.5",
      "M51 8L40 22.5",
      "M8 35H19L23 42.5H41L45 35H56",
      "M8 35V45A9 9 0 0 0 17 54H47A9 9 0 0 0 56 45V35",
    ],
    points: [[32, 25]],
  },
  mail: {
    lines: ["M53 44V40A13 13 0 0 0 40 27H12L24 15"],
    points: [[53, 52]],
  },
  meetings: {
    lines: [
      "M32.25 18V33L42.75 40A6.875 6.875 0 0 0 56.5 40V22.5A15 15 0 0 0 41.5 7.5H22.5A15 15 0 0 0 7.5 22.5V41.5A15 15 0 0 0 22.5 56.5H41",
    ],
    points: [[32.25, 33]],
  },
  tracking: {
    lines: ["M18 15V47L27 39L34 53L40.5 50L33.5 36H46.5L24 17.5"],
    points: [[16, 11]],
  },
  warmup: {
    lines: ["M7.5 47A24.5 24.5 0 0 1 56.5 47", "M32 47L44 30"],
    points: [[44, 30]],
  },
  // The teams Norbelys is built for (solutions.ts), each drawn as the object its page is built on.
  badge: {
    lines: [
      "M20 9H44A6 6 0 0 1 50 15V51A6 6 0 0 1 44 57H20A6 6 0 0 1 14 51V15A6 6 0 0 1 20 9Z",
      "M28 17H36",
      "M22 48C23.5 42 27.5 39 32 39S40.5 42 42 48",
    ],
    points: [[32, 30]],
    radius: 6,
  },
  bell: {
    lines: [
      "M32 7V13",
      "M13 46C17.5 42 19 37 19 31V26A13 13 0 0 1 45 26V31C45 37 46.5 42 51 46Z",
    ],
    points: [[32, 54]],
  },
  fader: {
    lines: [
      "M20 8V32",
      "M20 46V56",
      "M20 32A7 7 0 1 1 20 46A7 7 0 1 1 20 32",
      "M44 8V14",
      "M44 30V56",
    ],
    points: [[44, 22]],
    radius: 7,
  },
  frame: {
    lines: [
      "M17 13H47A5 5 0 0 1 52 18V48A5 5 0 0 1 47 53H17A5 5 0 0 1 12 48V18A5 5 0 0 1 17 13Z",
      "M22 23H42V43H22Z",
    ],
    points: [[52, 58]],
  },
  pawn: {
    lines: [
      "M24 27H40",
      "M26.5 27C26.5 35 24 41 19.5 46H44.5C40 41 37.5 35 37.5 27",
      "M15 54H49",
    ],
    points: [[32, 16]],
    radius: 6.5,
  },
  report: {
    lines: [
      "M13 21V50A7 7 0 0 0 20 57H39",
      "M28 7H44A7 7 0 0 1 51 14V42A7 7 0 0 1 44 49H28A7 7 0 0 1 21 42V14A7 7 0 0 1 28 7Z",
      "M28 39L34 32L39 36",
    ],
    points: [[44, 28]],
  },
  // Interface glyphs.
  arrow: { lines: ["M12 32H50", "M36 18L50 32L36 46"] },
  check: { lines: ["M13 33L26 46L51 19"] },
  chevron: { lines: ["M24 14L42 32L24 50"] },
  close: { lines: ["M17 17L47 47", "M47 17L17 47"] },
  copy: {
    lines: [
      "M24 32A8 8 0 0 1 32 24H46A8 8 0 0 1 54 32V46A8 8 0 0 1 46 54H32A8 8 0 0 1 24 46Z",
      "M12 38V20A8 8 0 0 1 20 12H38",
    ],
  },
  down: { lines: ["M14 24L32 42L50 24"] },
  external: { lines: ["M18 46L44 20", "M22 18H46V42"] },
  link: {
    lines: [
      "M28 36L36 28",
      "M30 22L35.5 16.5A9.9 9.9 0 0 1 49.5 30.5L44 36",
      "M34 42L28.5 47.5A9.9 9.9 0 0 1 14.5 33.5L20 28",
    ],
  },
  paperclip: {
    lines: [
      "M57 29.5L32.5 54A16 16 0 0 1 10 31.3L34.5 6.8A10.7 10.7 0 0 1 49.6 22L25 46.4A5.3 5.3 0 0 1 17.5 38.9L40.2 16.3",
    ],
  },
  plus: { lines: ["M32 12V52", "M12 32H52"] },
  print: {
    lines: [
      "M20 23V10H44V23",
      "M20 45H14A6 6 0 0 1 8 39V29A6 6 0 0 1 14 23H50A6 6 0 0 1 56 29V39A6 6 0 0 1 50 45H44",
      "M20 37H44V54H20Z",
    ],
  },
} as const satisfies Record<string, Drawing>;

export type GlyphName = keyof typeof GLYPHS;
