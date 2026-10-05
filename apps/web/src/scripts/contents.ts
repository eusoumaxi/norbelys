/**
 * A list of a page's sections (a post's contents, a product page's local menu) marks the one
 * being read: the last section whose heading has passed the top third of the screen. The list
 * works as plain links without this script.
 */

/** Starts following the reader through every section list on the page. */
export const startContents = (): void => {
  for (const contents of document.querySelectorAll<HTMLElement>(
    "[data-contents]"
  )) {
    const pairs = [
      ...contents.querySelectorAll<HTMLAnchorElement>("a[href^='#']"),
    ].flatMap((link) => {
      const id = decodeURIComponent(link.hash.slice(1));
      const target =
        id === ""
          ? null
          : document.querySelector<HTMLElement>(`#${CSS.escape(id)}`);
      return target ? [{ link, target }] : [];
    });
    if (pairs.length === 0) {
      continue;
    }
    let frame = 0;
    let current: HTMLAnchorElement | undefined;

    const update = (): void => {
      frame = 0;
      const line = window.innerHeight * 0.33;
      let reading: HTMLAnchorElement | undefined;
      for (const { link, target } of pairs) {
        if (target.getBoundingClientRect().top <= line) {
          reading = link;
        }
      }
      if (reading !== current) {
        current?.removeAttribute("aria-current");
        reading?.setAttribute("aria-current", "location");
        current = reading;
      }
    };
    update();
    window.addEventListener(
      "scroll",
      () => {
        if (frame === 0) {
          frame = requestAnimationFrame(update);
        }
      },
      { passive: true }
    );
  }
};
