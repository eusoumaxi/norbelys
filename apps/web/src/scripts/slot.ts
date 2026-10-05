/**
 * The solutions page's sentence: its slot turns to the next team every few seconds while it's in
 * view, and to whichever team the visitor points at or tabs to. Each turn swaps the word, the
 * object drawn beside it and its caption. Under reduced motion it never turns on its own.
 */

const TURN = 2800;
const RESUME = 4000;

/** Starts the slot, if the page has one. */
export const startSlot = (): void => {
  const hero = document.querySelector<HTMLElement>("[data-slot-hero]");
  if (!hero) {
    return;
  }
  const words = [...hero.querySelectorAll<HTMLElement>("[data-slot-word]")];
  const slugs = words.map((word) => word.dataset.slotWord ?? "");
  const parts = (slug: string): Element[] => [
    ...hero.querySelectorAll(
      `[data-slot-word="${slug}"], [data-slot-object="${slug}"], [data-slot-caption="${slug}"], [data-slot-link="${slug}"]`
    ),
  ];
  let current = 0;

  const show = (index: number): void => {
    if (index === current) {
      return;
    }
    // The word on its way out rises as the next one comes up from below.
    const leaving = words[current];
    leaving?.classList.add("is-leaving");
    window.setTimeout(() => leaving?.classList.remove("is-leaving"), 700);
    for (const part of parts(slugs[current] ?? "")) {
      part.classList.remove("is-on");
    }
    current = index;
    for (const part of parts(slugs[current] ?? "")) {
      part.classList.add("is-on");
    }
  };

  const motion = document.documentElement.classList.contains("motion");
  let timer = 0;
  let visible = false;
  let held = false;
  const run = (): void => {
    window.clearInterval(timer);
    timer = 0;
    if (motion && visible && !held && document.visibilityState === "visible") {
      timer = window.setInterval(
        () => show((current + 1) % slugs.length),
        TURN
      );
    }
  };

  let resume = 0;
  const hold = (index: number): void => {
    window.clearTimeout(resume);
    held = true;
    show(index);
    run();
  };
  const release = (): void => {
    window.clearTimeout(resume);
    resume = window.setTimeout(() => {
      held = false;
      run();
    }, RESUME);
  };
  for (const link of hero.querySelectorAll<HTMLElement>("[data-slot-link]")) {
    const index = slugs.indexOf(link.dataset.slotLink ?? "");
    link.addEventListener("pointerenter", () => hold(index));
    link.addEventListener("focus", () => hold(index));
    link.addEventListener("pointerleave", release);
    link.addEventListener("blur", release);
  }

  new IntersectionObserver((entries) => {
    visible = entries.some((entry) => entry.isIntersecting);
    run();
  }).observe(hero);
  document.addEventListener("visibilitychange", run);
};
