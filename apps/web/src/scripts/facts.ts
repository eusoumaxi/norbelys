/**
 * The AI page's demo: tick the facts Norbelys may use and the first line changes to match.
 * The lines are written in advance for the demo; with no facts ticked, the line is left out,
 * the way the product does it. Without this script the default line simply stays.
 */

/** Wires the facts demo, if the page has one. */
export const startFacts = (): void => {
  const demo = document.querySelector<HTMLElement>("[data-facts]");
  if (!demo) {
    return;
  }
  const boxes = [
    ...demo.querySelectorAll<HTMLInputElement>("input[type='checkbox']"),
  ];
  const lines = [...demo.querySelectorAll<HTMLElement>("[data-line]")];
  const show = (): void => {
    const picked = boxes.filter((box) => box.checked).map((box) => box.value);
    const key = picked.length > 0 ? picked.join("+") : "none";
    for (const line of lines) {
      const match = line.dataset.line === key;
      line.hidden = !match;
      if (match) {
        // Restart the typing reveal for the line that just came in.
        line.classList.remove("is-typing");
        line.getBoundingClientRect();
        line.classList.add("is-typing");
      }
    }
  };
  for (const box of boxes) {
    box.addEventListener("change", show);
  }
};
