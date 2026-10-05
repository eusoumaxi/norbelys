/**
 * Writes every brand file from `geometry.ts` and `illustrations.ts`: the logos, the marks, the
 * favicon and app icons as SVG, the illustrations, and the
 * rasterizer page that turns the SVGs into PNG and ICO files (`rasterize.html`).
 *
 *     bun brand/build.ts
 *
 * The PNG and ICO files are not written here, since that needs a renderer: serve the folder
 * (`bunx serve brand`), open `rasterize.html` and press "Download all".
 */
import { mkdir } from "node:fs/promises";
import path from "node:path";

import { COLORS, GRID, LOCKUP, MARK } from "./geometry.ts";
import { ILLUSTRATIONS, toSvg } from "./illustrations.ts";
import { lockupMarkup, markMarkup, svgElement as svg } from "./markup.ts";
import { renderRasterizer } from "./rasterize.ts";

const ROOT = import.meta.dir;

const write = async (file: string, content: string) => {
  const target = path.join(ROOT, file);
  await mkdir(path.dirname(target), { recursive: true });
  await Bun.write(target, `${content}\n`);
};

const LOGOS: [string, string, string][] = [
  ["logo/norbelys-logo-dark.svg", COLORS.pink, COLORS.white],
  ["logo/norbelys-logo-light.svg", COLORS.deep, COLORS.ink],
  ["logo/norbelys-logo-white.svg", COLORS.white, COLORS.white],
  ["logo/norbelys-logo-black.svg", COLORS.black, COLORS.black],
];
const MARKS: [string, string][] = [
  ["logo/norbelys-mark.svg", COLORS.pink],
  ["logo/norbelys-mark-deep.svg", COLORS.deep],
  ["logo/norbelys-mark-white.svg", COLORS.white],
  ["logo/norbelys-mark-black.svg", COLORS.black],
  ["logo/norbelys-mark-current.svg", "currentColor"],
];

/** The favicon follows the browser's theme and uses the heavier line of small sizes. */
const favicon = svg(
  GRID,
  GRID,
  "Norbelys",
  `<style>path{stroke:${COLORS.deep}}@media (prefers-color-scheme:dark){path{stroke:${COLORS.pink}}}</style>${markMarkup(COLORS.deep, MARK.smallStroke)}`
);

/** The app icon: a black tile with the pink mark at 62.5% of its side (iOS rounds the tile). */
const appIcon = svg(
  512,
  512,
  "Norbelys",
  `<rect width="512" height="512" fill="${COLORS.black}"/><g transform="translate(96 96) scale(5)">${markMarkup(COLORS.pink)}</g>`
);
/** The maskable icon keeps the mark inside the 80% safe circle Android crops to. */
const maskable = svg(
  512,
  512,
  "Norbelys",
  `<rect width="512" height="512" fill="${COLORS.black}"/><g transform="translate(128 128) scale(4)">${markMarkup(COLORS.pink)}</g>`
);

/** The web app manifest the icons belong to. */
const manifest = JSON.stringify(
  {
    background_color: COLORS.black,
    display: "standalone",
    icons: [
      { sizes: "192x192", src: "/icon-192.png", type: "image/png" },
      { sizes: "512x512", src: "/icon-512.png", type: "image/png" },
      {
        purpose: "maskable",
        sizes: "512x512",
        src: "/maskable-512.png",
        type: "image/png",
      },
    ],
    name: "Norbelys",
    short_name: "Norbelys",
    start_url: "/",
    theme_color: COLORS.black,
  },
  null,
  2
);

const FILES: [string, string][] = [
  ...LOGOS.map(([file, mark, word]): [string, string] => [
    file,
    svg(LOCKUP.width, LOCKUP.height, "Norbelys", lockupMarkup(mark, word)),
  ]),
  ...MARKS.map(([file, color]): [string, string] => [
    file,
    svg(GRID, GRID, "Norbelys", markMarkup(color)),
  ]),
  ["icons/favicon.svg", favicon],
  ["icons/app-icon.svg", appIcon],
  ["icons/maskable.svg", maskable],
  ["icons/site.webmanifest", manifest],
  ...ILLUSTRATIONS.map((illustration): [string, string] => [
    `illustrations/${illustration.name}.svg`,
    toSvg(illustration),
  ]),
  ["rasterize.html", renderRasterizer()],
];

await Promise.all(FILES.map(([file, content]) => write(file, content)));
