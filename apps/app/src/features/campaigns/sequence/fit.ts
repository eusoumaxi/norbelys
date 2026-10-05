/** The nearest ancestor that scrolls vertically: the content panel the page scrolls in. */
export const scroller = (element: HTMLElement): HTMLElement | null => {
  for (let node = element.parentElement; node; node = node.parentElement) {
    const { overflowY } = getComputedStyle(node);
    if (overflowY === "auto" || overflowY === "scroll") {
      return node;
    }
  }
  return null;
};

/**
 * A ref callback that keeps `--fit-height` on its element at the height left between the
 * element's top and the bottom of the panel the page scrolls in (less the bottom padding of the
 * boxes between them). An editor laid out with `h-(--fit-height)` then fills the screen and its
 * panes scroll on their own, whatever the header above it holds. Measured again whenever the
 * panel (the window) or the element's parent (the header above it) changes size.
 */
export const fitToPanel = (element: HTMLElement | null) => {
  const panel = element ? scroller(element) : null;
  if (!element || !panel) {
    return;
  }
  const measure = () => {
    const top =
      element.getBoundingClientRect().top -
      panel.getBoundingClientRect().top -
      panel.clientTop +
      panel.scrollTop;
    let below = 0;
    for (
      let node = element.parentElement;
      node && node !== panel;
      node = node.parentElement
    ) {
      // A computed padding is in pixels: `32px`.
      below +=
        Number(getComputedStyle(node).paddingBottom.replace("px", "")) || 0;
    }
    const height = Math.max(0, Math.floor(panel.clientHeight - top - below));
    element.style.setProperty("--fit-height", `${height}px`);
  };
  measure();
  const observer = new ResizeObserver(measure);
  observer.observe(panel);
  if (element.parentElement) {
    observer.observe(element.parentElement);
  }
  return () => observer.disconnect();
};
