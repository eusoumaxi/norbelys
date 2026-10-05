/**
 * The rows of roles drift with the scroll (the scene engine moves them); this leans them a little
 * into the direction of travel, then lets them settle back. It only listens while they can be seen.
 */

const MAX_DELTA = 60;

/** Starts the rows' lean, if motion is welcome. */
export const startMarquee = (): void => {
  const marquee = document.querySelector<HTMLElement>("[data-marquee]");
  if (!marquee || !document.documentElement.classList.contains("motion")) {
    return;
  }
  let last = window.scrollY;
  let lean = 0;
  let frame = 0;
  let visible = false;

  const step = (): void => {
    frame = 0;
    const current = window.scrollY;
    const delta = Math.max(-MAX_DELTA, Math.min(MAX_DELTA, current - last));
    last = current;
    lean = lean * 0.85 + delta * 0.15;
    marquee.style.setProperty("--lean", `${(lean * -0.12).toFixed(2)}deg`);
    if (visible && Math.abs(lean) > 0.02) {
      frame = requestAnimationFrame(step);
    }
  };
  const wake = (): void => {
    if (visible && frame === 0) {
      frame = requestAnimationFrame(step);
    }
  };

  new IntersectionObserver((entries) => {
    visible = entries.some((entry) => entry.isIntersecting);
    last = window.scrollY;
    wake();
  }).observe(marquee);
  window.addEventListener("scroll", wake, { passive: true });
};
