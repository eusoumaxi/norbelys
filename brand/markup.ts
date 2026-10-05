/**
 * The mark and the logo as SVG markup, for the files `build.ts` writes and the pages it renders.
 * Applications that draw the mark themselves (the dashboard's React component) read the same
 * path from `geometry.ts`.
 */
import { LOCKUP, MARK, markPath, WORDMARK } from "./geometry.ts";

/** The mark: one stroked line. `pathLength` lets CSS draw it without measuring it. */
export const markMarkup = (color: string, stroke: number = MARK.stroke) =>
  `<path class="nb-line" pathLength="1" d="${markPath()}" fill="none" stroke="${color}" stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round"/>`;

/** The horizontal logo, in its 333 × 80 box: the mark scaled 1.25, then the word. */
export const lockupMarkup = (markColor: string, wordColor: string) =>
  `<g transform="scale(${LOCKUP.markScale})">${markMarkup(markColor)}</g><path class="nb-word" d="${WORDMARK.d}" transform="translate(${LOCKUP.wordX} 0)" fill="${wordColor}"/>`;

/** An `<svg>` element around `body`. */
export const svgElement = (
  width: number,
  height: number,
  title: string,
  body: string,
  attributes = ""
) =>
  `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-label="${title}" ${attributes}><title>${title}</title>${body}</svg>`;
