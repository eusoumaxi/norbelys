/**
 * The page's main buttons lean a few pixels towards a mouse that comes close, and settle back
 * when it leaves. Touch screens and reduced motion get plain buttons.
 */

const PULL = 0.22;
const REACH = 6;

const clamp = (value: number): number =>
  Math.max(-REACH, Math.min(REACH, value));

/** Starts the magnetic buttons, if motion is welcome and a mouse is present. */
export const startMagnetic = (): void => {
  const fine = window.matchMedia("(hover: hover) and (pointer: fine)");
  if (!document.documentElement.classList.contains("motion")) {
    return;
  }
  for (const button of document.querySelectorAll<HTMLElement>(
    ".hero-actions .button, .closing-actions .button, .header-start"
  )) {
    button.addEventListener("pointermove", (event) => {
      if (!fine.matches) {
        return;
      }
      const bounds = button.getBoundingClientRect();
      const x = (event.clientX - bounds.left - bounds.width / 2) * PULL;
      const y = (event.clientY - bounds.top - bounds.height / 2) * PULL;
      button.style.translate = `${clamp(x).toFixed(1)}px ${clamp(y).toFixed(1)}px`;
    });
    button.addEventListener("pointerleave", () => {
      button.style.translate = "";
    });
  }
};
