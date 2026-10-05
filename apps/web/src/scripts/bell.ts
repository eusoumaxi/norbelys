/**
 * The SDR page's bell. It hangs on a spring, its clapper on another; a tap swings the bell, a tug
 * on the lanyard swings the clapper, and when the clapper meets the bell's wall the bell rings:
 * the sound draws round its mouth, the next reply comes out of it and the week's tally gets a
 * mark. It rings on its own three times while the hero is in view, then waits for the visitor.
 *
 * Under reduced motion nothing swings: a tap simply brings the next reply and the next mark.
 */

/** Pivots, in the drawing's own units (components/teams/sdr-bell.astro). */
const BELL_PIVOT = { x: 360, y: 124 } as const;
const CLAPPER_PIVOT = { x: 360, y: 300 } as const;

/** Springs per second squared, damping per second, and the clapper's room inside the bell. */
const BELL = { damping: 2.1, stiffness: 38 } as const;
const CLAPPER = { damping: 1.4, stiffness: 64 } as const;
const ROOM = 0.15;
const BOUNCE = 0.45;
/** How hard the clapper must meet the wall to ring it, and how hard a tap pushes the bell. */
const STRIKE = 0.55;
const PUSH = 1.7;
/** At most one reply per ring, however many times the clapper bounces. */
const REPLY_GAP = 900;
const AUTO_RINGS = [1500, 4600, 8200] as const;
const MAX_MARKS = 15;

interface Swing {
  bell: number;
  bellSpeed: number;
  clapper: number;
  clapperSpeed: number;
}

const degrees = (radians: number): number =>
  Math.round(((radians * 180) / Math.PI) * 100) / 100;

/** Draws one more mark on the tally: four upright, the fifth a pink stroke through them. */
const addMark = (tally: SVGGElement, count: number): void => {
  const group = Math.floor(count / 5);
  const place = count % 5;
  const x = 450 + group * 52 + place * 9;
  const mark = document.createElementNS("http://www.w3.org/2000/svg", "path");
  mark.setAttribute("pathLength", "1");
  if (place === 4) {
    mark.setAttribute("class", "bell-mark bell-mark-strike is-new");
    mark.setAttribute("d", `M${x - 41} 656L${x - 4} 622`);
  } else {
    mark.setAttribute("class", "bell-mark is-new");
    mark.setAttribute("d", `M${x} 618V658`);
  }
  tally.append(mark);
};

/** Starts the bell, if the page has one. */
export const startBell = (): void => {
  const hero = document.querySelector<HTMLElement>("[data-bell]");
  const swingGroup = hero?.querySelector<SVGGElement>("[data-bell-swing]");
  const clapperGroup = hero?.querySelector<SVGGElement>("[data-bell-clapper]");
  const art = hero?.querySelector<SVGSVGElement>(".bell-art");
  const tally = hero?.querySelector<SVGGElement>("[data-bell-tally]");
  const hit = hero?.querySelector<HTMLButtonElement>("[data-bell-hit]");
  const pull = hero?.querySelector<HTMLButtonElement>("[data-bell-pull]");
  const stage = hero?.querySelector<HTMLElement>(".bell-stage");
  if (
    !(
      hero &&
      swingGroup &&
      clapperGroup &&
      art &&
      tally &&
      hit &&
      pull &&
      stage
    )
  ) {
    return;
  }
  const replies = [...hero.querySelectorAll<HTMLElement>("[data-bell-reply]")];
  const motion = document.documentElement.classList.contains("motion");

  // The replies in view, newest first, and the one that comes out next.
  const shown = replies
    .map((reply, index) => ({ index, slot: Number(reply.dataset.slot) }))
    .filter((reply) => !Number.isNaN(reply.slot))
    .toSorted((a, b) => a.slot - b.slot)
    .map((reply) => reply.index);
  let next = shown.length % replies.length;
  let marks = Number(tally.dataset.count ?? 0);
  let lastReply = 0;

  const nextReply = (): void => {
    const now = performance.now();
    if (now - lastReply < REPLY_GAP) {
      return;
    }
    lastReply = now;
    shown.unshift(next);
    next = (next + 1) % replies.length;
    for (const [index, reply] of replies.entries()) {
      const slot = shown.indexOf(index);
      reply.classList.toggle("is-new", slot === 0 && motion);
      reply.dataset.slot = slot === -1 || slot > 2 ? "out" : String(slot);
    }
    while (shown.length > 3) {
      shown.pop();
    }
    if (marks < MAX_MARKS) {
      addMark(tally, marks);
      marks += 1;
    }
  };

  const sound = (): void => {
    stage.classList.remove("is-ringing");
    // Reading the layout restarts the waves' animation from its first frame.
    void stage.offsetWidth;
    stage.classList.add("is-ringing");
  };

  const ring = (): void => {
    sound();
    nextReply();
  };

  if (!motion) {
    const quiet = (): void => nextReply();
    hit.addEventListener("click", quiet);
    pull.addEventListener("click", quiet);
    return;
  }

  const swing: Swing = { bell: 0, bellSpeed: 0, clapper: 0, clapperSpeed: 0 };
  let dragging = false;
  let frame = 0;
  let last = 0;

  const draw = (): void => {
    swingGroup.setAttribute(
      "transform",
      `rotate(${degrees(swing.bell)} ${BELL_PIVOT.x} ${BELL_PIVOT.y})`
    );
    clapperGroup.setAttribute(
      "transform",
      `rotate(${degrees(swing.clapper - swing.bell)} ${CLAPPER_PIVOT.x} ${CLAPPER_PIVOT.y})`
    );
  };

  const step = (time: number): void => {
    frame = 0;
    const dt = last === 0 ? 1 / 60 : Math.min(0.05, (time - last) / 1000);
    last = time;
    swing.bellSpeed +=
      (-BELL.stiffness * swing.bell - BELL.damping * swing.bellSpeed) * dt;
    swing.bell += swing.bellSpeed * dt;
    if (!dragging) {
      swing.clapperSpeed +=
        (-CLAPPER.stiffness * swing.clapper -
          CLAPPER.damping * swing.clapperSpeed) *
        dt;
      swing.clapper += swing.clapperSpeed * dt;
    }
    // The clapper meets the bell's wall: it bounces back, and a hard enough meeting rings.
    const inside = swing.clapper - swing.bell;
    if (Math.abs(inside) > ROOM) {
      const side = Math.sign(inside);
      swing.clapper = swing.bell + side * ROOM;
      const closing = (swing.clapperSpeed - swing.bellSpeed) * side;
      if (closing > 0) {
        swing.clapperSpeed = swing.bellSpeed - closing * BOUNCE * side;
        swing.bellSpeed += closing * 0.18 * side;
        if (closing > STRIKE) {
          ring();
        }
      }
    }
    draw();
    const moving =
      dragging ||
      Math.abs(swing.bell) + Math.abs(swing.clapper) > 0.0008 ||
      Math.abs(swing.bellSpeed) + Math.abs(swing.clapperSpeed) > 0.004;
    if (moving) {
      frame = requestAnimationFrame(step);
    } else {
      swing.bell = 0;
      swing.clapper = 0;
      draw();
      last = 0;
    }
  };
  const run = (): void => {
    if (frame === 0) {
      frame = requestAnimationFrame(step);
    }
  };

  let direction = 1;
  const push = (): void => {
    swing.bellSpeed += PUSH * direction;
    direction *= -1;
    run();
  };

  // The rings it makes on its own, until the visitor rings it themselves.
  let timers: number[] = [];
  const stopAuto = (): void => {
    for (const timer of timers) {
      window.clearTimeout(timer);
    }
    timers = [];
  };
  const touched = (): void => {
    stopAuto();
    hero.classList.add("is-touched");
  };
  let started = false;
  new IntersectionObserver((entries) => {
    const visible = entries.some((entry) => entry.isIntersecting);
    if (visible && !started && !hero.classList.contains("is-touched")) {
      started = true;
      timers = AUTO_RINGS.map((delay) => window.setTimeout(push, delay));
    } else if (!visible) {
      stopAuto();
    }
  }).observe(stage);

  hit.addEventListener("click", () => {
    touched();
    push();
  });

  // The lanyard swings the clapper: drag it sideways and let go, or press it to swing it.
  const toDrawing = (event: PointerEvent): DOMPoint | undefined => {
    const matrix = art.getScreenCTM();
    return matrix
      ? new DOMPoint(event.clientX, event.clientY).matrixTransform(
          matrix.inverse()
        )
      : undefined;
  };
  let pressed: { x: number; moved: boolean } | undefined;
  pull.addEventListener("pointerdown", (event) => {
    pressed = { moved: false, x: event.clientX };
    pull.setPointerCapture(event.pointerId);
  });
  pull.addEventListener("pointermove", (event) => {
    if (!pressed) {
      return;
    }
    if (!pressed.moved && Math.abs(event.clientX - pressed.x) < 6) {
      return;
    }
    pressed.moved = true;
    dragging = true;
    touched();
    const point = toDrawing(event);
    if (point) {
      // The angle from the clapper's pivot to the pointer, measured from straight down.
      const angle = Math.atan2(
        CLAPPER_PIVOT.x - point.x,
        point.y - CLAPPER_PIVOT.y
      );
      swing.clapper = Math.max(-0.7, Math.min(0.7, angle));
      swing.clapperSpeed = 0;
    }
    run();
  });
  const release = (): void => {
    if (pressed && !pressed.moved) {
      touched();
      swing.clapperSpeed += 2.6 * direction;
      direction *= -1;
    }
    pressed = undefined;
    dragging = false;
    run();
  };
  pull.addEventListener("pointerup", release);
  pull.addEventListener("pointercancel", release);
  pull.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      touched();
      swing.clapperSpeed += 2.6 * direction;
      direction *= -1;
      run();
    }
  });
};
