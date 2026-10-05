/**
 * The hero's stack of replies: every few seconds the next one arrives on top. It rests while the
 * hero is out of view or the tab is hidden, and never moves for a visitor who prefers reduced
 * motion.
 */

const INTERVAL = 2600;

/** Starts the stack, if motion is welcome. */
export const startNotes = (): void => {
  const stack = document.querySelector<HTMLElement>("[data-notes]");
  if (!stack || !document.documentElement.classList.contains("motion")) {
    return;
  }
  const notes = [...stack.querySelectorAll<HTMLElement>("[data-note]")];
  const total = notes.length;
  let tick = 0;
  let timer = 0;
  let visible = true;

  const advance = (): void => {
    tick += 1;
    for (const [index, note] of notes.entries()) {
      note.dataset.slot = String((((tick - index) % total) + total) % total);
    }
  };
  const run = (): void => {
    window.clearInterval(timer);
    timer = 0;
    if (visible && document.visibilityState === "visible") {
      timer = window.setInterval(advance, INTERVAL);
    }
  };

  new IntersectionObserver((entries) => {
    visible = entries.some((entry) => entry.isIntersecting);
    run();
  }).observe(stack);
  document.addEventListener("visibilitychange", run);
};

/** Lets the hero's satellites lean towards a mouse, a few pixels at most. */
export const startParallax = (): void => {
  const hero = document.querySelector<HTMLElement>(".hero");
  const fine = window.matchMedia("(hover: hover) and (pointer: fine)");
  if (!hero || !document.documentElement.classList.contains("motion")) {
    return;
  }
  let frame = 0;
  let x = 0;
  let y = 0;
  hero.addEventListener("pointermove", (event) => {
    if (!fine.matches) {
      return;
    }
    const bounds = hero.getBoundingClientRect();
    x = ((event.clientX - bounds.left) / bounds.width) * 2 - 1;
    y = ((event.clientY - bounds.top) / bounds.height) * 2 - 1;
    if (frame === 0) {
      frame = requestAnimationFrame(() => {
        frame = 0;
        hero.style.setProperty("--mx", x.toFixed(3));
        hero.style.setProperty("--my", y.toFixed(3));
      });
    }
  });
  hero.addEventListener("pointerleave", () => {
    hero.style.setProperty("--mx", "0");
    hero.style.setProperty("--my", "0");
  });
};
