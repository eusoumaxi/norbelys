/**
 * The developers page's email: a pink point that travels the page's two drawings.
 *
 * - The hero: the point goes into the closed box, comes out at Alex and comes back as a reply,
 *   again and again while the box is on screen; the box's ports light as it passes them.
 * - The map: the visitor picks what happens (send, retry, reply, your server down) and the point
 *   plays it hop by hop, the hops it uses drawn darker, its step lit in the list beside it, and
 *   a tag saying what the API answers at each stop.
 *
 * A story is composed once into a timeline (the point's glides along the drawing, and the cues
 * that happen at given moments) and one animation frame loop plays it, so stopping is just not
 * asking for the next frame. Nothing here is needed to read the page: the steps are written
 * out, and without motion the map still marks the hops of the chosen story.
 */

/** The point moving along a path, between two lengths, over a stretch of the timeline. */
interface Glide {
  readonly start: number;
  readonly end: number;
  readonly path: SVGPathElement;
  readonly from: number;
  readonly to: number;
}

/** Something that happens at a moment of a story. */
type Cue =
  | { readonly kind: "step"; readonly step: number }
  | { readonly kind: "tag"; readonly text: string | null }
  | { readonly kind: "ping" }
  | { readonly kind: "refuse"; readonly on: boolean }
  | { readonly kind: "show"; readonly on: boolean }
  | { readonly kind: "place"; readonly x: number; readonly y: number };

interface Timeline {
  readonly glides: readonly Glide[];
  readonly cues: readonly { readonly at: number; readonly cue: Cue }[];
  readonly length: number;
}

/** One thing the point does in a story, as the stories below are written. */
type Move =
  | { readonly go: string }
  | { readonly back: string }
  | { readonly jump: string }
  | { readonly step: number }
  | { readonly tag: string | null }
  | { readonly wait: number }
  | { readonly refuse: true }
  | { readonly ping: true };

/** The hops each story uses, and what the point does in it. */
const STORIES: Readonly<
  Record<
    string,
    { readonly hops: readonly string[]; readonly moves: readonly Move[] }
  >
> = {
  assistant: {
    hops: ["assistant", "s2", "s3", "s4"],
    moves: [
      { jump: "assistant" },
      { step: 0 },
      { tag: "messages.create" },
      { wait: 700 },
      { tag: null },
      { go: "assistant" },
      { step: 1 },
      { tag: "202 queued" },
      { go: "s2" },
      { tag: null },
      { step: 2 },
      { go: "s3" },
      { go: "s4" },
      { ping: true },
      { wait: 1400 },
    ],
  },
  down: {
    hops: ["s7", "s8"],
    moves: [
      { jump: "postgres" },
      { step: 0 },
      { go: "s7" },
      { go: "s8" },
      { refuse: true },
      { tag: "503" },
      { wait: 700 },
      { step: 1 },
      { tag: "again in 5 s" },
      { back: "s8" },
      { go: "s8" },
      { refuse: true },
      { tag: "again in 5 min" },
      { back: "s8" },
      { go: "s8" },
      { refuse: true },
      { tag: "again in 30 min" },
      { back: "s8" },
      { go: "s8" },
      { step: 2 },
      { ping: true },
      { tag: "200" },
      { wait: 1600 },
      { tag: null },
    ],
  },
  reply: {
    hops: ["s5", "s6", "s7", "s8"],
    moves: [
      { jump: "alex" },
      { step: 0 },
      { tag: "reply" },
      { go: "s5" },
      { tag: null },
      { step: 1 },
      { go: "s6" },
      { wait: 500 },
      { step: 2 },
      { go: "s7" },
      { go: "s8" },
      { ping: true },
      { tag: "inbound_message.received" },
      { wait: 1800 },
      { tag: null },
    ],
  },
  retry: {
    hops: ["s1"],
    moves: [
      { jump: "app" },
      { step: 0 },
      { go: "s1" },
      { step: 1 },
      { tag: "replayed" },
      { wait: 1300 },
      { step: 2 },
      { back: "s1" },
      { ping: true },
      { tag: "202, again" },
      { wait: 1600 },
      { tag: null },
    ],
  },
  send: {
    hops: ["s1", "s2", "s3", "s4", "s7", "s8"],
    moves: [
      { jump: "app" },
      { step: 0 },
      { go: "s1" },
      { step: 1 },
      { tag: "202 queued" },
      { go: "s2" },
      { tag: null },
      { wait: 300 },
      { step: 2 },
      { go: "s3" },
      { go: "s4" },
      { ping: true },
      { wait: 600 },
      { step: 3 },
      { jump: "postgres" },
      { go: "s7" },
      { go: "s8" },
      { ping: true },
      { tag: "message.sent" },
      { wait: 1800 },
      { tag: null },
    ],
  },
};

const easeInOut = (t: number): number =>
  t < 0.5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2;

const place = (point: SVGCircleElement, x: number, y: number): void => {
  point.setAttribute("cx", x.toFixed(2));
  point.setAttribute("cy", y.toFixed(2));
};

const visible = (element: Element): boolean =>
  element.getBoundingClientRect().width > 0;

/** Where a glide has the point at a moment inside it. */
const pointOn = (glide: Glide, time: number): DOMPoint => {
  const t = Math.min(
    1,
    Math.max(0, (time - glide.start) / (glide.end - glide.start))
  );
  return glide.path.getPointAtLength(
    glide.from + (glide.to - glide.from) * easeInOut(t)
  );
};

/**
 * Plays a timeline: moves the point along its glides, hands each cue to `apply` when its moment
 * comes and calls `done` at the end. Returns the way to stop it.
 */
const perform = (
  timeline: Timeline,
  point: SVGCircleElement,
  apply: (cue: Cue) => void,
  done: () => void,
  passing?: (x: number, y: number) => void
): (() => void) => {
  let cue = 0;
  let glide = 0;
  let frame = 0;
  const start = performance.now();
  const tick = (now: number): void => {
    const time = now - start;
    for (
      ;
      cue < timeline.cues.length && (timeline.cues[cue]?.at ?? 0) <= time;
      cue += 1
    ) {
      const next = timeline.cues[cue];
      if (next) {
        apply(next.cue);
      }
    }
    for (
      ;
      glide < timeline.glides.length &&
      (timeline.glides[glide]?.end ?? 0) <= time;
      glide += 1
    ) {
      const ended = timeline.glides[glide];
      if (ended) {
        const at = pointOn(ended, ended.end);
        place(point, at.x, at.y);
      }
    }
    const current = timeline.glides[glide];
    if (current && current.start <= time) {
      const at = pointOn(current, time);
      place(point, at.x, at.y);
      passing?.(at.x, at.y);
    }
    if (time < timeline.length) {
      frame = requestAnimationFrame(tick);
    } else {
      done();
    }
  };
  frame = requestAnimationFrame(tick);
  return () => cancelAnimationFrame(frame);
};

/** A ring that spreads from where the point is: something arrived. */
const ping = (point: SVGCircleElement): void => {
  const ring = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  for (const name of ["cx", "cy", "r"]) {
    ring.setAttribute(name, point.getAttribute(name) ?? "0");
  }
  ring.setAttribute("class", "box-ping");
  point.before(ring);
  ring.addEventListener("animationend", () => ring.remove());
};

/** Plays `loop` whenever `target` is on screen and the page is in front; stops it otherwise. */
const whileShown = (target: Element, loop: () => () => void): void => {
  let stop: (() => void) | undefined;
  let shown = false;
  const update = (): void => {
    const go = shown && !document.hidden;
    if (go && !stop) {
      stop = loop();
    } else if (!go && stop) {
      stop();
      stop = undefined;
    }
  };
  new IntersectionObserver((entries) => {
    shown = entries.some((entry) => entry.isIntersecting);
    update();
  }).observe(target);
  document.addEventListener("visibilitychange", update);
};

/** Lights each port for a moment when the point passes within reach of it. */
const portLighter = (svg: SVGSVGElement): ((x: number, y: number) => void) => {
  const ports = [...svg.querySelectorAll<SVGCircleElement>("[data-port]")].map(
    (port) => ({
      port,
      x: Number(port.getAttribute("cx")),
      y: Number(port.getAttribute("cy")),
    })
  );
  return (x, y) => {
    for (const { port, x: px, y: py } of ports) {
      if (
        Math.hypot(px - x, py - y) < 16 &&
        !port.classList.contains("is-lit")
      ) {
        port.classList.add("is-lit");
        window.setTimeout(() => port.classList.remove("is-lit"), 260);
      }
    }
  };
};

const nothing = (): void => undefined;

/** One lap of the hero: in on a cable, over the top, out; then back along the bottom. */
const heroLap = (out: SVGPathElement, back: SVGPathElement): Timeline => {
  const there = out.getTotalLength();
  const home = back.getTotalLength();
  const going = Math.min(3800, Math.max(2600, there * 2.2));
  const coming = Math.min(3800, Math.max(2600, home * 2.2));
  const start = back.getPointAtLength(0);
  const leave = going + 700;
  const turn = leave + 220;
  const end = turn + coming;
  return {
    cues: [
      { at: going, cue: { kind: "ping" } },
      { at: leave, cue: { kind: "show", on: false } },
      { at: turn, cue: { kind: "place", x: start.x, y: start.y } },
      { at: turn, cue: { kind: "show", on: true } },
      { at: end, cue: { kind: "ping" } },
    ],
    glides: [
      { end: going, from: 0, path: out, start: 0, to: there },
      { end, from: 0, path: back, start: turn, to: home },
    ],
    length: end + 1600,
  };
};

/**
 * The hero: an email comes in on one of the cables, rides the box's edge out along the top and
 * comes back along the bottom as a reply; each lap takes the next cable in.
 */
const startHero = (): void => {
  const svg = document.querySelector<SVGSVGElement>(".dev-hero-box");
  const point = svg?.querySelector<SVGCircleElement>("[data-hero-point]");
  const back = svg?.querySelector<SVGPathElement>("[data-hero-back]");
  const outs = svg
    ? [...svg.querySelectorAll<SVGPathElement>("[data-hero-out]")]
    : [];
  if (!svg || !point || !back || outs.length === 0) {
    return;
  }
  const passing = portLighter(svg);
  const apply = (cue: Cue): void => {
    if (cue.kind === "ping") {
      ping(point);
    } else if (cue.kind === "show") {
      point.classList.toggle("is-hidden", !cue.on);
    } else if (cue.kind === "place") {
      place(point, cue.x, cue.y);
    }
  };
  let lap = 0;
  // The box draws itself before the first lap.
  let first = true;
  whileShown(svg, () => {
    let stop = nothing;
    let wait = 0;
    const run = (): void => {
      const out = outs[lap % outs.length];
      if (!visible(svg) || !out) {
        wait = window.setTimeout(run, 600);
        return;
      }
      lap += 1;
      const begin = out.getPointAtLength(0);
      place(point, begin.x, begin.y);
      point.classList.remove("is-hidden");
      stop = perform(heroLap(out, back), point, apply, run, passing);
    };
    wait = window.setTimeout(run, first ? 1700 : 300);
    first = false;
    return () => {
      window.clearTimeout(wait);
      stop();
    };
  });
};

/** A hop a drawing may leave out, and the one that stands in for it there. */
const STAND_INS: Readonly<Record<string, string>> = { assistant: "s1" };

/** The hop of a drawing that a story's hop names: itself, or its stand-in. */
const hopIn = (svg: SVGSVGElement, hop: string): string =>
  svg.querySelector(`[data-route="${hop}"]`) ? hop : (STAND_INS[hop] ?? hop);

/** Where a stop of the map sits in a drawing. */
const stopAt = (
  svg: SVGSVGElement,
  name: string
): readonly [number, number] => {
  const marker = svg.querySelector<SVGCircleElement>(`[data-stop="${name}"]`);
  return [
    Number(marker?.getAttribute("cx")),
    Number(marker?.getAttribute("cy")),
  ];
};

/** Turns a story's moves into a timeline on one drawing. */
const compose = (svg: SVGSVGElement, moves: readonly Move[]): Timeline => {
  const glides: Glide[] = [];
  const cues: { at: number; cue: Cue }[] = [];
  let time = 0;
  const travel = (hop: string, forward: boolean): void => {
    // The drawn hops carry pathLength="1" for their dashes; the point follows the copies in
    // <defs>, measured in the drawing's own units.
    const path = svg.querySelector<SVGPathElement>(
      `[data-route="${hopIn(svg, hop)}"]`
    );
    if (!path) {
      return;
    }
    const length = path.getTotalLength();
    const duration = Math.min(1300, Math.max(520, length * 2.4));
    if (!forward) {
      cues.push({ at: time, cue: { kind: "refuse", on: false } });
    }
    glides.push({
      end: time + duration,
      from: forward ? 0 : length,
      path,
      start: time,
      to: forward ? length : 0,
    });
    time += duration;
  };
  for (const move of moves) {
    if ("go" in move) {
      travel(move.go, true);
    } else if ("back" in move) {
      travel(move.back, false);
    } else if ("jump" in move) {
      const [x, y] = stopAt(svg, move.jump);
      cues.push({ at: time, cue: { kind: "show", on: false } });
      time += 220;
      cues.push(
        { at: time, cue: { kind: "place", x, y } },
        { at: time, cue: { kind: "show", on: true } }
      );
    } else if ("refuse" in move) {
      cues.push({ at: time, cue: { kind: "refuse", on: true } });
      time += 420;
    } else if ("wait" in move) {
      time += move.wait;
    } else if ("step" in move) {
      cues.push({ at: time, cue: { kind: "step", step: move.step } });
    } else if ("tag" in move) {
      cues.push({ at: time, cue: { kind: "tag", text: move.tag } });
    } else {
      cues.push({ at: time, cue: { kind: "ping" } });
    }
  }
  return { cues, glides, length: time };
};

/** Shows a tag beside the point, or hides it: what the API answered at this stop. */
const tagOn = (
  svg: SVGSVGElement,
  point: SVGCircleElement,
  text: string | null
): void => {
  const tag = svg.querySelector<SVGGElement>("[data-tag]");
  const words = tag?.querySelector<SVGTextElement>("text");
  const plate = tag?.querySelector<SVGRectElement>("rect");
  if (!tag || !words || !plate) {
    return;
  }
  if (text === null) {
    tag.classList.remove("is-on");
    return;
  }
  words.textContent = text;
  const width = words.getComputedTextLength() + 30;
  plate.setAttribute("x", (-width / 2).toFixed(1));
  plate.setAttribute("width", width.toFixed(1));
  const x = Number(point.getAttribute("cx"));
  const y = Number(point.getAttribute("cy"));
  const box = svg.viewBox.baseVal;
  // Above the point, or beside it near either end of a wide drawing, where the names are; at
  // the top, above the names of the ways in; at the foot of a tall one, to the left of Alex,
  // clear of tracking's line. Always inside the drawing's width.
  const wide = box.width > box.height;
  const edge = box.width * 0.2;
  let at: readonly [number, number] = [x, y - 34];
  if (wide && x < box.x + edge) {
    at = [x + width / 2 + 20, y];
  } else if (wide && x > box.x + box.width - edge) {
    at = [x - width / 2 - 20, y];
  } else if (wide && y < box.y + 60) {
    at = [x, y - 58];
  } else if (!wide && y > box.y + box.height - 100) {
    at = [x - width / 2 - 18, y];
  }
  const left = Math.min(
    Math.max(at[0], box.x + width / 2 + 4),
    box.x + box.width - width / 2 - 4
  );
  tag.setAttribute(
    "transform",
    `translate(${left.toFixed(1)} ${at[1].toFixed(1)})`
  );
  tag.classList.remove("is-on");
  tag.getBoundingClientRect();
  tag.classList.add("is-on");
};

/** Darkens the hops a story uses in every drawing and lights its step in the list. */
const mark = (
  map: HTMLElement,
  drawings: readonly SVGSVGElement[],
  story: string,
  step: number | undefined
): void => {
  for (const svg of drawings) {
    const hops = new Set(
      (STORIES[story]?.hops ?? []).map((hop) => hopIn(svg, hop))
    );
    svg.dataset.story = story;
    for (const hop of svg.querySelectorAll<SVGPathElement>(
      ".map-hop, .map-branch"
    )) {
      hop.classList.toggle("is-used", hops.has(hop.dataset.hop ?? ""));
    }
  }
  const list = map.querySelector(`[data-steps-for="${CSS.escape(story)}"]`);
  for (const [index, item] of [...(list?.children ?? [])].entries()) {
    if (index === step) {
      item.setAttribute("aria-current", "step");
    } else {
      item.removeAttribute("aria-current");
    }
  }
};

/** The map: the visitor picks a story, the point plays it. */
const startMap = (): void => {
  const map = document.querySelector<HTMLElement>("[data-map]");
  if (!map) {
    return;
  }
  const motion = document.documentElement.classList.contains("motion");
  const drawings = [...map.querySelectorAll<SVGSVGElement>("[data-drawing]")];
  const choices = [
    ...map.querySelectorAll<HTMLButtonElement>("[data-scenario]"),
  ];
  const replay = map.querySelector<HTMLButtonElement>("[data-replay]");
  let current = choices[0]?.dataset.scenario ?? "send";
  let stop: (() => void) | undefined;
  let playing: SVGSVGElement | undefined;

  const play = (story: string): void => {
    stop?.();
    stop = undefined;
    current = story;
    mark(map, drawings, story, motion ? undefined : 0);
    const svg = drawings.find(visible);
    const point = svg?.querySelector<SVGCircleElement>("[data-point]");
    const moves = STORIES[story]?.moves;
    if (!motion || !svg || !point || !moves) {
      return;
    }
    playing = svg;
    if (replay) {
      replay.hidden = true;
    }
    tagOn(svg, point, null);
    point.classList.remove("is-refused", "is-hidden");
    const apply = (cue: Cue): void => {
      if (cue.kind === "step") {
        mark(map, drawings, story, cue.step);
      } else if (cue.kind === "tag") {
        tagOn(svg, point, cue.text);
      } else if (cue.kind === "ping") {
        ping(point);
      } else if (cue.kind === "refuse") {
        point.classList.toggle("is-refused", cue.on);
      } else if (cue.kind === "show") {
        point.classList.toggle("is-hidden", !cue.on);
      } else {
        place(point, cue.x, cue.y);
      }
    };
    stop = perform(compose(svg, moves), point, apply, () => {
      point.classList.remove("is-refused");
      stop = undefined;
      if (replay) {
        replay.hidden = false;
      }
    });
  };

  map.addEventListener("tabs:select", () => {
    const chosen = choices.find(
      (choice) => choice.getAttribute("aria-selected") === "true"
    );
    play(chosen?.dataset.scenario ?? "send");
  });

  replay?.addEventListener("click", () => play(current));
  mark(map, drawings, current, motion ? undefined : 0);
  if (!motion) {
    return;
  }
  // The first story plays by itself, once, when the map is well in view.
  const observer = new IntersectionObserver(
    (entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        observer.disconnect();
        window.setTimeout(() => play(current), 600);
      }
    },
    { threshold: 0.6 }
  );
  observer.observe(map.querySelector(".map-stage") ?? map);
  // A drawing swapped for the other mid-story starts the story again on the new one.
  window.addEventListener("resize", () => {
    const shown = drawings.find(visible);
    if (stop && shown !== playing) {
      play(current);
    }
  });
};

/** Starts both drawings, when the page has them and motion is welcome. */
export const startBox = (): void => {
  if (document.documentElement.classList.contains("motion")) {
    startHero();
  }
  startMap();
};
