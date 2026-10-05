/**
 * The 404 page's scene: envelopes thrown at the giant 4@4 bounce off its lines, tumble and pile
 * up on the ground. The lines are read from the drawings themselves, so an envelope hits exactly
 * what is drawn. Click or tap to send another, drag one to throw it.
 *
 * Each envelope is a small rigid card: its corners hit the drawing's lines, the drawing's points
 * press into its sides, and envelopes push each other apart like coins. Without motion the same
 * envelopes are simply there, at rest; without this script the page is whole, just still.
 */

/** How finely time is cut: small steps keep a fast envelope from passing through a line. */
const STEP = 1 / 300;
/** The most envelopes in the scene; past it, the oldest fades away to make room. */
const MAX = 36;
/** How bouncy the drawing and the ground are, and how much they grip. */
const BOUNCE = 0.45;
const GROUND_BOUNCE = 0.3;
const GRIP = 0.45;
/** How long a hit's pink ring lasts, and a leaving envelope's fade, in seconds. */
const RING = 0.6;
const FADE = 0.45;

/** What the attempts counter says as the bounces pile up. */
const NOTES = [
  [3, "Norbelys would have stopped after the first one."],
  [10, "Ten tries. You’d make a very persistent SDR."],
  [25, "Twenty-five. Some addresses just don’t exist."],
] as const;

interface Envelope {
  x: number;
  y: number;
  vx: number;
  vy: number;
  angle: number;
  spin: number;
  /**
   * Where it was when it last moved more than a hair, and for how long it hasn't since: past a
   * moment it sleeps until something wakes it. Measured by distance, not speed, so a card that
   * only trembles on a slope still falls asleep.
   */
  rest: { x: number; y: number; angle: number };
  still: number;
  asleep: boolean;
  /** Whether it has hit the drawing yet: its first hit counts as an attempt. */
  bounced: boolean;
  /** 1 while it stays, falling to 0 as it fades out to make room. */
  life: number;
  leaving: boolean;
  /** Where it has just been while flying fast: the dotted line of a message on its way. */
  trail: { x: number; y: number }[];
}

/** A point on the drawing's line, and which glyph it belongs to. */
interface Dot {
  readonly x: number;
  readonly y: number;
  readonly glyph: number;
}

/** A short piece of the drawing's line, between two dots. */
interface Piece {
  readonly a: Dot;
  readonly b: Dot;
}

interface Ring {
  readonly x: number;
  readonly y: number;
  age: number;
}

const clamp = (low: number, value: number, high: number): number =>
  Math.min(high, Math.max(low, value));
const random = (low: number, high: number): number =>
  low + Math.random() * (high - low);

/** A new envelope at (x, y), on its way at (vx, vy), tilted and turning a little at random. */
const envelopeAt = (
  x: number,
  y: number,
  vx: number,
  vy: number
): Envelope => ({
  angle: random(-0.4, 0.4),
  asleep: false,
  bounced: false,
  leaving: false,
  life: 1,
  rest: { angle: 0, x, y },
  spin: random(-6, 6),
  still: 0,
  trail: [],
  vx,
  vy,
  x,
  y,
});

/** Wires the scene, if the page has one. */
export const startBounce = (): void => {
  const stage = document.querySelector<HTMLElement>("[data-bounce]");
  const canvas = stage?.querySelector<HTMLCanvasElement>(
    "[data-bounce-canvas]"
  );
  const context = canvas?.getContext("2d");
  if (!(stage && canvas && context)) {
    return;
  }
  const moving = document.documentElement.classList.contains("motion");
  const glyphs = [
    ...stage.querySelectorAll<SVGSVGElement>("[data-bounce-glyph]"),
  ];
  const counter = document.querySelector<HTMLElement>("[data-bounce-count]");
  const note = document.querySelector<HTMLElement>("[data-bounce-note]");
  const recipient = document.querySelector<HTMLElement>("[data-bounce-path]");
  const send = document.querySelector<HTMLButtonElement>("[data-bounce-send]");
  const hint = stage.querySelector<HTMLElement>("[data-bounce-hint]");
  if (recipient) {
    recipient.textContent = `${location.host || "norbelys.com"}${location.pathname}`;
  }

  const style = getComputedStyle(document.documentElement);
  const color = (name: string, fallback: string): string =>
    style.getPropertyValue(name).trim() || fallback;
  const ink = color("--ink", "#0a0a0a");
  const paper = color("--white", "#ffffff");
  const pink = color("--pink-hot", "#ec4899");

  const envelopes: Envelope[] = [];
  const rings: Ring[] = [];
  let dots: Dot[] = [];
  let pieces: Piece[] = [];
  let width = 0;
  let height = 0;
  /** A glyph's size on screen, the scene's unit: gravity and envelopes scale with it. */
  let size = 1;
  /** Half the drawing's line, half an envelope's width and height, its spin's inertia. */
  let thick = 1;
  let halfWidth = 1;
  let halfHeight = 1;
  let inertia = 1;
  let gravity = 1;
  let attempts = 0;
  let held: Envelope | undefined;
  let grip = { x: 0, y: 0 };
  let pull = { x: 0, y: 0 };
  let moved = 0;
  /** Where the mouse is over the stage, when a click there would drop an envelope. */
  let hover: { x: number; y: number } | undefined;
  let frame = 0;
  let last = 0;
  let carry = 0;
  let visible = true;

  /** Reads the drawings' lines as dots and pieces, in the canvas's own pixels. */
  const measure = (): void => {
    width = canvas.clientWidth;
    height = canvas.clientHeight;
    const ratio = Math.min(2, devicePixelRatio || 1);
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    const origin = canvas.getBoundingClientRect();
    dots = [];
    pieces = [];
    for (const [index, glyph] of glyphs.entries()) {
      const box = glyph.getBoundingClientRect();
      const view = glyph.viewBox.baseVal;
      const unit = box.width / view.width;
      size = box.width;
      // The drawn line is 6 units wide; it pushes back from 3 units either side of its centre.
      thick = 3 * unit;
      for (const source of glyph.querySelectorAll<SVGPathElement>(
        "path:not(.mark-comet)"
      )) {
        // A plain copy of the line, measured in its own units (pathLength would rescale them).
        const probe = document.createElementNS(
          "http://www.w3.org/2000/svg",
          "path"
        );
        probe.setAttribute("d", source.getAttribute("d") ?? "");
        probe.setAttribute("visibility", "hidden");
        glyph.append(probe);
        const total = probe.getTotalLength();
        const count = Math.max(2, Math.ceil(total / 2.5));
        let previous: Dot | undefined;
        for (let step = 0; step <= count; step += 1) {
          const point = probe.getPointAtLength((total * step) / count);
          const dot = {
            glyph: index,
            x: box.left - origin.left + (point.x - view.x) * unit,
            y: box.top - origin.top + (point.y - view.y) * unit,
          };
          dots.push(dot);
          if (previous) {
            pieces.push({ a: previous, b: dot });
          }
          previous = dot;
        }
        probe.remove();
      }
    }
    halfWidth = clamp(14, size * 0.105, 40);
    halfHeight = halfWidth * 0.66;
    inertia = (4 * halfWidth * halfWidth + 4 * halfHeight * halfHeight) / 12;
    gravity = size * 6.5;
    for (const envelope of envelopes) {
      envelope.x = clamp(halfWidth, envelope.x, width - halfWidth);
      envelope.y = Math.min(envelope.y, height - halfHeight);
      envelope.asleep = false;
    }
  };

  /** One more attempt that bounced: count it, and let the drawing feel it. */
  const hit = (
    x: number,
    y: number,
    nx: number,
    ny: number,
    glyph: number
  ): void => {
    attempts += 1;
    if (counter) {
      counter.textContent = String(attempts);
    }
    const [, said] = NOTES.findLast(([from]) => attempts >= from) ?? [];
    if (note && said) {
      note.textContent = said;
    }
    if (moving) {
      // The notice keeps count: the number jumps, in pink, with each new attempt.
      counter?.animate([{ color: pink, scale: 1.5 }, { scale: 1 }], {
        duration: 420,
        easing: "cubic-bezier(0.16, 1, 0.3, 1)",
      });
      rings.push({ age: 0, x, y });
      // The line gives a little, away from the envelope that struck it.
      glyphs[glyph]?.animate(
        [
          { translate: "0 0" },
          { translate: `${(-nx * size) / 70}px ${(-ny * size) / 70}px` },
          { translate: "0 0" },
        ],
        { duration: 340, easing: "cubic-bezier(0.2, 0.8, 0.2, 1)" }
      );
    }
  };

  /**
   * Pushes an envelope out of whatever it touched at (px, py), along the normal (nx, ny) by
   * `depth`, and answers the touch with a bounce and some grip, turning it as a card would.
   */
  const push = (
    envelope: Envelope,
    px: number,
    py: number,
    nx: number,
    ny: number,
    depth: number,
    bounce: number,
    glyph: number
  ): void => {
    envelope.x += nx * depth;
    envelope.y += ny * depth;
    const rx = px - envelope.x;
    const ry = py - envelope.y;
    const vx = envelope.vx - envelope.spin * ry;
    const vy = envelope.vy + envelope.spin * rx;
    const along = vx * nx + vy * ny;
    if (along >= 0) {
      return;
    }
    // A slow touch is a resting one: no bounce, so a resting envelope doesn't shiver.
    const spring = along < -size * 0.25 ? bounce : 0;
    const turn = rx * ny - ry * nx;
    const impulse = (-(1 + spring) * along) / (1 + (turn * turn) / inertia);
    envelope.vx += impulse * nx;
    envelope.vy += impulse * ny;
    envelope.spin += (turn * impulse) / inertia;
    const tx = -ny;
    const ty = nx;
    const slide = vx * tx + vy * ty;
    const twist = rx * ty - ry * tx;
    const friction = clamp(
      -GRIP * impulse,
      -slide / (1 + (twist * twist) / inertia),
      GRIP * impulse
    );
    envelope.vx += friction * tx;
    envelope.vy += friction * ty;
    envelope.spin += (twist * friction) / inertia;
    // Paper doesn't rock for long: a card at rest on something loses its wobble quickly.
    if (spring === 0) {
      envelope.spin *= 0.97;
      envelope.vx *= 0.995;
      envelope.vy *= 0.995;
    }
    if (glyph >= 0 && !envelope.bounced && along < -size * 0.6) {
      envelope.bounced = true;
      hit(px, py, nx, ny, glyph);
    }
  };

  /** Whether an envelope is still on its way in from beyond a side of the scene. */
  const entering = (envelope: Envelope): boolean =>
    (envelope.x < halfWidth * 1.5 && envelope.vx > 0) ||
    (envelope.x > width - halfWidth * 1.5 && envelope.vx < 0);

  /** Keeps an envelope out of the drawing, the ground and the sides. */
  const collide = (envelope: Envelope): void => {
    const cos = Math.cos(envelope.angle);
    const sin = Math.sin(envelope.angle);
    const reach = Math.hypot(halfWidth, halfHeight) + thick;
    const sides = !entering(envelope);
    for (const [lx, ly] of [
      [-halfWidth, -halfHeight],
      [halfWidth, -halfHeight],
      [halfWidth, halfHeight],
      [-halfWidth, halfHeight],
    ] as const) {
      const cx = envelope.x + cos * lx - sin * ly;
      const cy = envelope.y + sin * lx + cos * ly;
      if (cy > height) {
        push(envelope, cx, height, 0, -1, cy - height, GROUND_BOUNCE, -1);
      }
      if (sides && cx < 0) {
        push(envelope, 0, cy, 1, 0, -cx, BOUNCE, -1);
      }
      if (sides && cx > width) {
        push(envelope, width, cy, -1, 0, cx - width, BOUNCE, -1);
      }
      for (const { a, b } of pieces) {
        if (
          Math.abs(a.x - envelope.x) > reach * 2 ||
          Math.abs(a.y - envelope.y) > reach * 2
        ) {
          continue;
        }
        const ex = b.x - a.x;
        const ey = b.y - a.y;
        const t = clamp(
          0,
          ((cx - a.x) * ex + (cy - a.y) * ey) / (ex * ex + ey * ey || 1),
          1
        );
        const dx = cx - (a.x + ex * t);
        const dy = cy - (a.y + ey * t);
        const distance = Math.hypot(dx, dy);
        if (distance < thick && distance > 0) {
          push(
            envelope,
            cx,
            cy,
            dx / distance,
            dy / distance,
            thick - distance,
            BOUNCE,
            a.glyph
          );
        }
      }
    }
    // The drawing's points pressing into the envelope's sides, between its corners.
    for (const dot of dots) {
      const dx = dot.x - envelope.x;
      const dy = dot.y - envelope.y;
      if (Math.abs(dx) > reach || Math.abs(dy) > reach) {
        continue;
      }
      const lx = dx * cos + dy * sin;
      const ly = -dx * sin + dy * cos;
      const overX = halfWidth + thick - Math.abs(lx);
      const overY = halfHeight + thick - Math.abs(ly);
      if (overX <= 0 || overY <= 0) {
        continue;
      }
      // Out along the nearer side, away from the point.
      const [ux, uy, depth] =
        overX < overY ? [-Math.sign(lx), 0, overX] : [0, -Math.sign(ly), overY];
      push(
        envelope,
        dot.x,
        dot.y,
        ux * cos - uy * sin,
        ux * sin + uy * cos,
        depth,
        BOUNCE,
        dot.glyph
      );
    }
  };

  /** Envelopes push each other apart, like coins; a held or sleeping one stands its ground. */
  const jostle = (): void => {
    const reach = halfHeight * 2.1;
    for (const [index, first] of envelopes.entries()) {
      for (const second of envelopes.slice(index + 1)) {
        const dx = second.x - first.x;
        const dy = second.y - first.y;
        const distance = Math.hypot(dx, dy);
        if (distance >= reach || distance === 0) {
          continue;
        }
        const nx = dx / distance;
        const ny = dy / distance;
        const firstFree = first !== held && !first.asleep;
        const secondFree = second !== held && !second.asleep;
        if (!(firstFree || secondFree)) {
          continue;
        }
        // Half the overlap each step: a pile settles over a few steps instead of shoving.
        const overlap = (reach - distance) * 0.5;
        const share = firstFree && secondFree ? 0.5 : 1;
        if (firstFree) {
          first.x -= nx * overlap * share;
          first.y -= ny * overlap * share;
        }
        if (secondFree) {
          second.x += nx * overlap * share;
          second.y += ny * overlap * share;
        }
        const closing =
          (second.vx - first.vx) * nx + (second.vy - first.vy) * ny;
        if (closing < 0) {
          const impulse = -1.2 * closing * share;
          if (firstFree) {
            first.vx -= impulse * nx;
            first.vy -= impulse * ny;
          }
          if (secondFree) {
            second.vx += impulse * nx;
            second.vy += impulse * ny;
          }
          // A hard knock wakes a sleeper.
          if (closing < -size * 0.5) {
            first.asleep = false;
            second.asleep = false;
          }
        }
      }
    }
  };

  const advance = (dt: number): void => {
    for (const envelope of envelopes) {
      if (envelope === held || envelope.asleep) {
        continue;
      }
      envelope.vy += gravity * dt;
      envelope.vx *= 0.9995;
      envelope.vy *= 0.9995;
      envelope.spin *= 0.998;
      envelope.x += envelope.vx * dt;
      envelope.y += envelope.vy * dt;
      envelope.angle += envelope.spin * dt;
      collide(envelope);
      const drift = Math.hypot(
        envelope.x - envelope.rest.x,
        envelope.y - envelope.rest.y
      );
      if (
        drift > size * 0.006 ||
        Math.abs(envelope.angle - envelope.rest.angle) > 0.04
      ) {
        envelope.rest = { angle: envelope.angle, x: envelope.x, y: envelope.y };
        envelope.still = 0;
      } else {
        envelope.still += dt;
      }
      if (envelope.still > 0.4) {
        envelope.asleep = true;
        envelope.vx = 0;
        envelope.vy = 0;
        envelope.spin = 0;
      }
    }
    jostle();
  };

  const draw = (): void => {
    context.clearRect(0, 0, width, height);
    // Each envelope's shadow on the ground, darker as it comes down.
    for (const envelope of envelopes) {
      const near = clamp(0, 1 - (height - envelope.y) / (size * 0.9), 1);
      if (near > 0) {
        context.globalAlpha = 0.09 * near * envelope.life;
        context.fillStyle = ink;
        context.beginPath();
        context.ellipse(
          envelope.x,
          height,
          halfWidth * (0.7 + near * 0.4),
          halfHeight * (0.12 + near * 0.1),
          0,
          0,
          Math.PI * 2
        );
        context.fill();
      }
    }
    context.lineWidth = 2;
    context.strokeStyle = pink;
    for (const ring of rings) {
      const t = ring.age / RING;
      context.globalAlpha = 1 - t;
      context.beginPath();
      context.arc(ring.x, ring.y, halfWidth * (0.2 + t * 1.3), 0, Math.PI * 2);
      context.stroke();
    }
    const line = Math.max(1.5, halfWidth * 0.085);
    context.fillStyle = pink;
    for (const envelope of envelopes) {
      for (const [index, point] of envelope.trail.entries()) {
        const t = (index + 1) / envelope.trail.length;
        context.globalAlpha = 0.6 * t * envelope.life;
        context.beginPath();
        context.arc(point.x, point.y, line * (0.5 + t * 0.8), 0, Math.PI * 2);
        context.fill();
      }
    }
    context.globalAlpha = 1;
    context.lineWidth = line;
    context.lineJoin = "round";
    context.lineCap = "round";
    /** One envelope; a draft (where a click would drop one) is only its dashed outline. */
    const paint = (
      x: number,
      y: number,
      angle: number,
      alpha: number,
      draft: boolean
    ): void => {
      context.save();
      context.globalAlpha = alpha;
      context.translate(x, y);
      context.rotate(angle);
      context.setLineDash(draft ? [line * 2, line * 2.4] : []);
      context.beginPath();
      context.roundRect(
        -halfWidth,
        -halfHeight,
        halfWidth * 2,
        halfHeight * 2,
        halfHeight * 0.3
      );
      if (!draft) {
        context.fillStyle = paper;
        context.fill();
      }
      context.strokeStyle = ink;
      context.stroke();
      context.beginPath();
      context.moveTo(-halfWidth * 0.78, -halfHeight * 0.6);
      context.lineTo(0, halfHeight * 0.16);
      context.lineTo(halfWidth * 0.78, -halfHeight * 0.6);
      context.stroke();
      // The seal: the one pink point, where the flap closes.
      context.beginPath();
      context.arc(0, halfHeight * 0.16, line * 1.25, 0, Math.PI * 2);
      context.fillStyle = pink;
      context.fill();
      context.restore();
    };
    for (const envelope of envelopes) {
      paint(envelope.x, envelope.y, envelope.angle, envelope.life, false);
    }
    if (hover && !held) {
      paint(hover.x, hover.y, -0.08, 0.35, true);
    }
    context.globalAlpha = 1;
  };

  const busy = (): boolean =>
    held !== undefined ||
    rings.length > 0 ||
    envelopes.some(
      (envelope) =>
        !envelope.asleep || envelope.leaving || envelope.trail.length > 0
    );

  const tick = (now: number): void => {
    frame = 0;
    const elapsed = last === 0 ? STEP : Math.min(0.05, (now - last) / 1000);
    last = now;
    carry += elapsed;
    let steps = 0;
    while (carry >= STEP && steps < 12) {
      advance(STEP);
      carry -= STEP;
      steps += 1;
    }
    if (steps === 12) {
      carry = 0;
    }
    for (const ring of rings) {
      ring.age += elapsed;
    }
    // A fast envelope leaves dots behind it; a slow one lets them go, one a frame.
    for (const envelope of envelopes) {
      const fast = Math.hypot(envelope.vx, envelope.vy) > size * 1.1;
      const end = envelope.trail.at(-1);
      if (
        fast &&
        envelope !== held &&
        (!end ||
          Math.hypot(envelope.x - end.x, envelope.y - end.y) > halfWidth * 0.6)
      ) {
        envelope.trail.push({ x: envelope.x, y: envelope.y });
      }
      if (envelope.trail.length > 9 || (!fast && envelope.trail.length > 0)) {
        envelope.trail.shift();
      }
    }
    for (const envelope of envelopes) {
      if (envelope.leaving) {
        envelope.life -= elapsed / FADE;
      }
    }
    rings.splice(0, rings.length, ...rings.filter((ring) => ring.age < RING));
    envelopes.splice(
      0,
      envelopes.length,
      ...envelopes.filter((envelope) => envelope.life > 0)
    );
    draw();
    if (busy() && visible) {
      frame = requestAnimationFrame(tick);
    } else {
      last = 0;
    }
  };

  /** Without motion, the scene jumps straight to where everything comes to rest. */
  const settle = (): void => {
    for (let time = 0; time < 8 && busy(); time += STEP) {
      advance(STEP);
    }
    for (const envelope of envelopes) {
      envelope.life = envelope.leaving ? 0 : 1;
    }
    envelopes.splice(
      0,
      envelopes.length,
      ...envelopes.filter((envelope) => envelope.life > 0)
    );
    draw();
  };

  const wake = (): void => {
    if (!moving) {
      settle();
      return;
    }
    if (frame === 0 && visible) {
      last = 0;
      frame = requestAnimationFrame(tick);
    }
  };

  /** Wakes every envelope, so none is left resting on one that has gone. */
  const stir = (): void => {
    for (const envelope of envelopes) {
      envelope.asleep = false;
      envelope.still = 0;
    }
  };

  const add = (envelope: Envelope): void => {
    envelopes.push(envelope);
    const staying = envelopes.filter((each) => !each.leaving);
    const oldest = staying.length > MAX ? staying[0] : undefined;
    if (oldest) {
      oldest.leaving = true;
      stir();
    }
  };

  /**
   * Throws an envelope from a side of the scene at the @'s shoulder on that side: it arcs over the
   * 4, peaking just above the drawing, falls onto the @ and comes back the way it came, returned
   * to sender.
   */
  const toss = (from: "left" | "right"): void => {
    const mark = glyphs[1]?.getBoundingClientRect();
    const origin = canvas.getBoundingClientRect();
    if (!mark) {
      return;
    }
    const top = mark.top - origin.top;
    const across = from === "left" ? random(0.1, 0.4) : random(0.6, 0.9);
    const tx = mark.left - origin.left + mark.width * across;
    const ty = top + mark.height * random(0, 0.1);
    const sx = from === "left" ? -halfWidth * 2 : width + halfWidth * 2;
    const sy = top + mark.height * random(0.15, 0.45);
    // The arc's peak, above the drawing and below the page's header.
    const peak = Math.max(top - size * random(0.22, 0.34), height * 0.2);
    const rise = Math.sqrt(2 * gravity * Math.max(1, sy - peak));
    const flight =
      rise / gravity + Math.sqrt((2 * Math.max(1, ty - peak)) / gravity);
    add(envelopeAt(sx, sy, (tx - sx) / flight, -rise));
    wake();
  };

  const drop = (x: number, y: number): void => {
    add(envelopeAt(x, y, random(-0.2, 0.2) * size, random(-0.4, 0) * size));
    wake();
  };

  const pointer = (event: PointerEvent): { x: number; y: number } => {
    const origin = canvas.getBoundingClientRect();
    return { x: event.clientX - origin.left, y: event.clientY - origin.top };
  };

  /** Whether the pointer is under the page's header, which the stage runs beneath: not a place to play. */
  const header = document.querySelector<HTMLElement>("[data-header]");
  const beneath = (event: PointerEvent): boolean =>
    event.clientY < (header?.getBoundingClientRect().bottom ?? 0);

  /** The topmost envelope under a point, if any. */
  const under = (x: number, y: number): Envelope | undefined =>
    envelopes.findLast((envelope) => {
      const dx = x - envelope.x;
      const dy = y - envelope.y;
      const cos = Math.cos(envelope.angle);
      const sin = Math.sin(envelope.angle);
      return (
        Math.abs(dx * cos + dy * sin) < halfWidth * 1.15 &&
        Math.abs(-dx * sin + dy * cos) < halfHeight * 1.3
      );
    });

  const quiet = (): void => {
    if (hint) {
      hint.dataset.gone = "";
    }
  };

  canvas.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || beneath(event)) {
      return;
    }
    const at = pointer(event);
    quiet();
    const target = under(at.x, at.y);
    if (!target || !moving) {
      drop(at.x, at.y);
      return;
    }
    held = target;
    stir();
    grip = { x: target.x - at.x, y: target.y - at.y };
    pull = { x: 0, y: 0 };
    moved = event.timeStamp;
    canvas.setPointerCapture(event.pointerId);
    canvas.dataset.holding = "";
    wake();
  });
  canvas.addEventListener("pointermove", (event) => {
    const at = pointer(event);
    if (!held) {
      const over = under(at.x, at.y);
      canvas.dataset.over = over ? "envelope" : "";
      // A mouse sees the envelope its click would drop; a finger has nothing to hover with.
      hover =
        moving && event.pointerType === "mouse" && !over && !beneath(event)
          ? at
          : undefined;
      if (frame === 0) {
        draw();
      }
      return;
    }
    const x = at.x + grip.x;
    const y = at.y + grip.y;
    const dt = clamp(1 / 240, (event.timeStamp - moved) / 1000, 0.1);
    moved = event.timeStamp;
    // The throw: how fast the hand was moving, smoothed over the last few moves.
    pull = {
      x: pull.x * 0.6 + ((x - held.x) / dt) * 0.4,
      y: pull.y * 0.6 + ((y - held.y) / dt) * 0.4,
    };
    // In the hand, the envelope leans into the way it's being moved.
    const lean = clamp(-0.6, pull.x / (size * 12), 0.6);
    held.angle += (lean - held.angle) * 0.25;
    held.spin = 0;
    held.x = x;
    held.y = y;
  });
  const release = (event: PointerEvent): void => {
    if (!held) {
      return;
    }
    const fastest = size * 9;
    const speed = Math.hypot(pull.x, pull.y);
    const scale = speed > fastest ? fastest / speed : 1;
    held.vx = event.type === "pointercancel" ? 0 : pull.x * scale;
    held.vy = event.type === "pointercancel" ? 0 : pull.y * scale;
    held = undefined;
    delete canvas.dataset.holding;
    wake();
  };
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);
  canvas.addEventListener("pointerleave", () => {
    hover = undefined;
    if (frame === 0) {
      draw();
    }
  });

  if (send) {
    send.hidden = false;
    send.addEventListener("click", () => {
      quiet();
      toss(Math.random() < 0.5 ? "left" : "right");
    });
  }

  measure();
  new ResizeObserver(() => {
    measure();
    wake();
    if (!moving || frame === 0) {
      draw();
    }
  }).observe(stage);
  new IntersectionObserver(([entry]) => {
    visible = entry?.isIntersecting ?? true;
    if (visible) {
      wake();
    }
  }).observe(stage);

  // The opening: once the 4@4 has drawn itself, three attempts at the address, and three bounces.
  const opening = ["left", "right", "left"] as const;
  if (moving) {
    for (const [index, side] of opening.entries()) {
      window.setTimeout(() => toss(side), 1300 + index * 650);
    }
    window.setTimeout(() => {
      if (hint) {
        hint.hidden = false;
      }
    }, 3400);
  } else {
    for (const side of opening) {
      toss(side);
    }
    if (hint) {
      hint.hidden = false;
    }
  }
};
