import { STOPS, cameraAt, frame } from "../road";
import type { View } from "../road";

/**
 * Drives the campaigns page's road with the scroll. The road starts framed beside the pitch,
 * opens to the whole screen over the first stretch, then the camera glides from stop to stop
 * while the card for the stop in view comes up. Without motion this never runs, and the road
 * stays the picture the page was built with.
 */

const clamp = (value: number): number => Math.min(1, Math.max(0, value));
const mix = (from: number, to: number, t: number): number =>
  from + (to - from) * t;
const smooth = (t: number): number => t * t * (3 - 2 * t);

/** The road's lines, redrawn every frame, by the name the SVG gives them. */
const PATHS = [
  "dashes",
  "edges",
  "ramp",
  "rampEdges",
  "reflectors",
  "road",
] as const;

/** Where the framed road sits on the stage before it opens: the slot beside the pitch, or a card's inset. */
interface Box {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

/**
 * The lens for a box on the stage: the vanishing point a little above its middle, and a focal
 * length that fills it. A box clearly taller than wide (a phone) gets a longer lens, so it shows
 * the road, not the sky.
 */
const lensFor = (box: Box): View => {
  const tall = box.height > box.width * 1.3;
  return {
    cx: box.x + box.width / 2,
    cy: box.y + box.height * (tall ? 0.47 : 0.44),
    f: Math.min(box.height * 0.98, box.width * (tall ? 1.2 : 1.03)),
  };
};

/** Wires the road, if the page has one. */
export const startRoad = (): void => {
  const root = document.querySelector<HTMLElement>("[data-road]");
  if (!root || !document.documentElement.classList.contains("motion")) {
    return;
  }
  const head = root.querySelector<HTMLElement>("[data-road-head]");
  const slot = root.querySelector<HTMLElement>("[data-road-slot]");
  const track = root.querySelector<HTMLElement>("[data-road-track]");
  const stage = root.querySelector<HTMLElement>("[data-road-stage]");
  const view = root.querySelector<HTMLElement>("[data-road-view]");
  const svg = view?.querySelector("svg");
  if (!(head && track && stage && view && svg)) {
    return;
  }
  const part = (name: string): Element | null =>
    svg.querySelector(`[data-road="${name}"]`);
  const paths = PATHS.map((name) => [name, part(name)] as const);
  const horizon = part("horizon");
  const fog = part("fog");
  const sprites = [...svg.querySelectorAll<SVGGElement>("[data-sprite]")];
  const cards = [...root.querySelectorAll<HTMLElement>("[data-road-card]")];
  const last = STOPS.length - 1;

  let width = 0;
  let height = 0;
  let framed: Box = { height: 0, width: 0, x: 0, y: 0 };
  let shown = -1;
  let request = 0;

  const measure = (): void => {
    width = stage.clientWidth;
    height = stage.clientHeight;
    svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    if (slot && slot.offsetParent !== null) {
      // The slot's place when the page is at the top: beside the pitch, over the stage.
      const box = slot.getBoundingClientRect();
      framed = {
        height: box.height,
        width: box.width,
        x: box.left - stage.getBoundingClientRect().left,
        y: box.top - head.getBoundingClientRect().top,
      };
    } else {
      framed = { height: height - 32, width: width - 32, x: 16, y: 16 };
    }
  };

  /** How far through the drive the page has scrolled, in stops. */
  const goal = (): number => {
    const box = track.getBoundingClientRect();
    const range = box.height - innerHeight;
    return range > 0 ? clamp(-box.top / range) * last : 0;
  };

  const render = (position: number): void => {
    const opened = smooth(clamp(position / 0.85));
    const full = lensFor({ height, width, x: 0, y: 0 });
    const small = lensFor(framed);
    const lens: View = {
      cx: mix(small.cx, full.cx, opened),
      cy: mix(small.cy, full.cy, opened),
      f: mix(small.f, full.f, opened),
    };
    const next = frame(cameraAt(position), lens);
    for (const [name, element] of paths) {
      element?.setAttribute("d", next[name] || "M0 0");
    }
    horizon?.setAttribute("y1", String(next.horizon));
    horizon?.setAttribute("y2", String(next.horizon));
    fog?.setAttribute("y", String(next.horizon - 150));
    for (const [index, sprite] of sprites.entries()) {
      const state = next.sprites[index];
      if (state) {
        sprite.setAttribute("transform", state.transform);
        sprite.setAttribute("opacity", String(state.opacity));
        sprite.classList.toggle("is-up", state.up);
      }
    }
    const closed = 1 - opened;
    const radius = 36 * closed;
    view.style.clipPath = `inset(${(framed.y * closed).toFixed(1)}px ${((width - framed.x - framed.width) * closed).toFixed(1)}px ${((height - framed.y - framed.height) * closed).toFixed(1)}px ${(framed.x * closed).toFixed(1)}px round ${radius.toFixed(1)}px)`;
    const nearest = Math.round(position);
    for (const card of cards) {
      card.classList.toggle(
        "is-active",
        Number(card.dataset.roadCard) === nearest &&
          Math.abs(position - nearest) < 0.3
      );
    }
    root.style.setProperty("--opened", opened.toFixed(3));
  };

  const tick = (): void => {
    request = 0;
    const target = goal();
    // Glide towards where the scroll is, so a flick of the wheel becomes a drive.
    shown = shown < 0 ? target : shown + (target - shown) * 0.14;
    if (Math.abs(target - shown) < 0.0015) {
      shown = target;
    }
    render(shown);
    if (shown !== target) {
      request = requestAnimationFrame(tick);
    }
  };
  const wake = (): void => {
    if (request === 0) {
      request = requestAnimationFrame(tick);
    }
  };

  measure();
  tick();
  root.classList.add("is-driving");
  addEventListener("scroll", wake, { passive: true });
  addEventListener("resize", () => {
    measure();
    shown = -1;
    wake();
  });

  // "Take the drive" scrolls to the first stop instead of jumping to the cards.
  for (const link of root.querySelectorAll<HTMLAnchorElement>(
    "[data-road-to]"
  )) {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      const box = track.getBoundingClientRect();
      const stop = Number(link.dataset.roadTo) || 1;
      scrollTo({
        behavior: "smooth",
        top: scrollY + box.top + ((box.height - innerHeight) * stop) / last,
      });
    });
  }
};
