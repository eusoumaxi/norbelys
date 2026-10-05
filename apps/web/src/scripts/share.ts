/** The "Copy link" button copies the page's address and says so for a moment. */

const SAY_FOR = 2200;

/** Wires every copy-link button on the page. */
export const startShare = (): void => {
  for (const button of document.querySelectorAll<HTMLButtonElement>(
    "[data-copy-link]"
  )) {
    const label = button.textContent ?? "";
    let timer = 0;
    const copy = async (): Promise<void> => {
      const [address = window.location.href] = window.location.href.split("#");
      await navigator.clipboard.writeText(address);
      button.textContent = button.dataset.copied ?? label;
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        button.textContent = label;
      }, SAY_FOR);
    };
    button.addEventListener("click", () => {
      void copy();
    });
  }
};
