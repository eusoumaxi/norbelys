/**
 * The founders page's gallery. Each frame hangs a few degrees askew on its nail; the pointer (a
 * tap, on a phone, or focus) straightens it, with the small swing a frame makes when it's nudged
 * level, and it stays straight, the way a frame does once someone has fixed it. Pressing the empty
 * frame on the wall hangs the next reply in it: the work drops into the mat, the frame rocks on its
 * nail, the label changes and the sold dot goes on. After the last reply the frame is empty again,
 * waiting. If nobody has touched the wall a few seconds in, the empty frame rocks once to say it's
 * there.
 *
 * Further down, the audio tour: each note moves level with its stop in the framed email where
 * there's room, a leader line is drawn from stop to note, and pointing at either lights both.
 *
 * Under reduced motion every frame hangs straight and a press simply swaps the reply and label.
 */

/** A frame's spring, per second squared and per second: a quick settle with one small overshoot. */
const SPRING = { damping: 8, stiffness: 72 } as const;
/** How hard hanging a reply rocks the frame, in degrees a second. */
const KNOCK = 26;
/** When the empty frame rocks on its own, if the visitor hasn't touched the wall yet. */
const NUDGE_AFTER = 4200;

interface Swing {
  readonly frame: HTMLElement;
  angle: number;
  speed: number;
  target: number;
}

/** What can be done to a hung frame. */
interface Hands {
  /** Levels it; it swings a moment, then stays level. */
  readonly straighten: (frame: HTMLElement) => void;
  /** Rocks it on its nail, as hanging something in it would. */
  readonly knock: (frame: HTMLElement, strength: number) => void;
}

/** Restarts a CSS animation on an element by taking its class off and putting it back. */
const replay = (element: HTMLElement, name: string, on: boolean): void => {
  element.classList.remove(name);
  if (on) {
    // Reading the layout makes the browser see the class come back as new.
    void element.offsetWidth;
    element.classList.add(name);
  }
};

/**
 * Hangs every frame on the page on a spring around its nail, askew by its `--tilt` while motion
 * is welcome, and straightens each one the first time the pointer meets it.
 */
const hangFrames = (motion: boolean, touched: () => void): Hands => {
  const swings = new Map<HTMLElement, Swing>();
  for (const frame of document.querySelectorAll<HTMLElement>("[data-frame]")) {
    const tilt = Number(frame.dataset.tilt ?? 0);
    const angle = motion && Number.isFinite(tilt) ? tilt : 0;
    swings.set(frame, { angle, frame, speed: 0, target: angle });
  }

  let frameId = 0;
  let last = 0;
  const step = (time: number): void => {
    frameId = 0;
    const dt = last === 0 ? 1 / 60 : Math.min(0.05, (time - last) / 1000);
    last = time;
    let moving = false;
    for (const swing of swings.values()) {
      const off = swing.angle - swing.target;
      if (Math.abs(off) < 0.004 && Math.abs(swing.speed) < 0.02) {
        if (off !== 0) {
          swing.angle = swing.target;
          swing.speed = 0;
          swing.frame.style.rotate = `${swing.angle}deg`;
        }
        continue;
      }
      swing.speed +=
        (-SPRING.stiffness * off - SPRING.damping * swing.speed) * dt;
      swing.angle += swing.speed * dt;
      swing.frame.style.rotate = `${swing.angle.toFixed(3)}deg`;
      moving = true;
    }
    if (moving) {
      frameId = requestAnimationFrame(step);
    } else {
      last = 0;
    }
  };
  const run = (): void => {
    if (motion && frameId === 0) {
      frameId = requestAnimationFrame(step);
    }
  };

  const straighten = (frame: HTMLElement): void => {
    const swing = swings.get(frame);
    if (swing && swing.target !== 0) {
      swing.target = 0;
      run();
    }
  };
  let side = 1;
  const knock = (frame: HTMLElement, strength: number): void => {
    const swing = swings.get(frame);
    if (swing) {
      swing.speed += strength * side;
      side *= -1;
      run();
    }
  };

  if (motion) {
    for (const [frame, swing] of swings) {
      if (swing.target === 0) {
        continue;
      }
      const level = (): void => {
        if (swing.target !== 0) {
          touched();
        }
        straighten(frame);
      };
      // A mouse or a pen levels it on the way in. A finger levels it with a tap, not by
      // scrolling past it: a scroll cancels the touch before it lifts, so no pointerup comes.
      const near = (event: PointerEvent): void => {
        if (event.pointerType !== "touch") {
          level();
        }
      };
      frame.addEventListener("pointerenter", near);
      frame.addEventListener("pointerdown", near);
      frame.addEventListener("pointerup", level);
      // The keyboard reaches it too: the empty frame's button is inside it.
      frame.addEventListener("focusin", level);
    }
  }
  return { knock, straighten };
};

/** Starts the wall's empty frame: each press hangs the next reply, then it waits empty again. */
const startHang = (
  wall: HTMLElement,
  motion: boolean,
  hands: Hands,
  touched: () => boolean
): void => {
  const hang = wall.querySelector<HTMLElement>("[data-hang]");
  const button = hang?.querySelector<HTMLButtonElement>("[data-hang-button]");
  const frame = hang?.querySelector<HTMLElement>("[data-frame]");
  if (!(hang && button && frame)) {
    return;
  }
  const works = [...hang.querySelectorAll<HTMLElement>("[data-work]")];
  const labels = [...hang.querySelectorAll<HTMLElement>("[data-label]")];
  let current = 0;
  let pressed = false;

  const show = (index: number): void => {
    current = index;
    for (const [place, work] of works.entries()) {
      work.hidden = place !== index;
      replay(work, "is-new", motion && place === index);
    }
    for (const [place, label] of labels.entries()) {
      label.hidden = place !== index;
      replay(label, "is-new", motion && place === index);
    }
    button.setAttribute(
      "aria-label",
      index === works.length - 1
        ? "Take the reply down and leave the frame empty"
        : "Hang the next reply in the frame"
    );
  };

  button.addEventListener("click", () => {
    pressed = true;
    show((current + 1) % works.length);
    hands.straighten(frame);
    hands.knock(frame, KNOCK);
  });

  if (!motion) {
    return;
  }
  // One rock of the empty frame once the wall has been in view a while, if nobody's touched it.
  let timer = 0;
  new IntersectionObserver((entries) => {
    window.clearTimeout(timer);
    if (
      entries.some((entry) => entry.isIntersecting) &&
      !pressed &&
      !touched()
    ) {
      timer = window.setTimeout(() => {
        if (!pressed && !touched()) {
          hands.knock(frame, KNOCK * 0.7);
        }
      }, NUDGE_AFTER);
    }
  }).observe(frame);
};

/** Where the tour's leader lines bend: this far past the frame's outer edge. */
const BEND = 22;

const middle = (box: DOMRect): number => box.top + box.height / 2;

interface Stop {
  readonly index: string;
  readonly note: HTMLElement;
  readonly number: HTMLElement;
  readonly stop: HTMLElement;
  readonly rows: readonly HTMLElement[];
}

/** Draws the audio tour's leader lines and links each note to its stop, if the page has a tour. */
const startTour = (): void => {
  const room = document.querySelector<HTMLElement>("[data-tour] .gallery-room");
  const svg = room?.querySelector<SVGSVGElement>("[data-leaders]");
  const frame = room?.querySelector<HTMLElement>("[data-frame]");
  if (!(room && svg && frame)) {
    return;
  }
  const stops: Stop[] = [];
  for (const note of room.querySelectorAll<HTMLElement>("[data-note]")) {
    const index = note.dataset.note ?? "";
    const number = note.querySelector<HTMLElement>(".gallery-number");
    const stop = room.querySelector<HTMLElement>(`[data-stop="${index}"]`);
    if (number && stop) {
      const rows = [
        ...room.querySelectorAll<HTMLElement>(`[data-target="${index}"]`),
      ];
      stops.push({ index, note, number, rows, stop });
    }
  }
  const wide = window.matchMedia("(min-width: 1000px)");

  const layout = (): void => {
    for (const { note } of stops) {
      note.style.removeProperty("margin-top");
    }
    svg.replaceChildren();
    if (!wide.matches) {
      return;
    }
    // Each note comes down level with its stop, unless the note above is in the way.
    for (const { note, number, stop } of stops) {
      const gap =
        middle(stop.getBoundingClientRect()) -
        middle(number.getBoundingClientRect());
      if (gap > 0.5) {
        note.style.marginTop = `${gap}px`;
      }
    }
    const origin = room.getBoundingClientRect();
    const bend = frame.getBoundingClientRect().right - origin.left + BEND;
    for (const [order, { index, number, stop }] of stops.entries()) {
      const from = stop.getBoundingClientRect();
      const to = number.getBoundingClientRect();
      const x1 = from.right - origin.left + 5;
      const y1 = middle(from) - origin.top;
      const x2 = to.left - origin.left - 8;
      const y2 = middle(to) - origin.top;
      const path = document.createElementNS(
        "http://www.w3.org/2000/svg",
        "path"
      );
      const level = Math.abs(y2 - y1) < 1;
      path.setAttribute(
        "d",
        level
          ? `M${x1.toFixed(1)} ${y1.toFixed(1)}H${x2.toFixed(1)}`
          : `M${x1.toFixed(1)} ${y1.toFixed(1)}H${bend.toFixed(1)}L${(x2 - 18).toFixed(1)} ${y2.toFixed(1)}H${x2.toFixed(1)}`
      );
      path.setAttribute("pathLength", "1");
      path.dataset.leader = index;
      path.style.setProperty("--k", String(order));
      svg.append(path);
    }
  };

  const light = (index?: string): void => {
    if (index === undefined) {
      delete room.dataset.lit;
    } else {
      room.dataset.lit = index;
    }
    for (const stop of stops) {
      const on = stop.index === index;
      stop.note.classList.toggle("is-lit", on);
      stop.stop.classList.toggle("is-lit", on);
      for (const row of stop.rows) {
        row.classList.toggle("is-lit", on);
      }
    }
    for (const path of svg.querySelectorAll<SVGPathElement>("path")) {
      path.classList.toggle("is-lit", path.dataset.leader === index);
    }
  };
  for (const stop of stops) {
    for (const element of [stop.note, ...stop.rows]) {
      element.addEventListener("pointerenter", () => light(stop.index));
      element.addEventListener("pointerleave", () => light());
    }
    stop.note.addEventListener("focus", () => light(stop.index));
    stop.note.addEventListener("blur", () => light());
  }

  let queued = 0;
  const relayout = (): void => {
    if (queued === 0) {
      queued = requestAnimationFrame(() => {
        queued = 0;
        layout();
      });
    }
  };
  new ResizeObserver(relayout).observe(room);
  wide.addEventListener("change", relayout);
  // The notes' heights settle once the page's font has loaded.
  const afterFonts = async (): Promise<void> => {
    await document.fonts.ready;
    relayout();
  };
  void afterFonts();
};

/** Starts the founders page's gallery: its frames, the wall's empty frame and the audio tour. */
export const startGallery = (): void => {
  const motion = document.documentElement.classList.contains("motion");
  let touched = false;
  const hands = hangFrames(motion, () => {
    touched = true;
  });
  const wall = document.querySelector<HTMLElement>("[data-wall]");
  if (wall) {
    startHang(wall, motion, hands, () => touched);
  }
  startTour();
};
