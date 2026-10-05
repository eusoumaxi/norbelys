/**
 * The legal binder: a narrow rail of tabs opens on the current one, the print button appears
 * (printing needs this script), and every clause heading gets a button that copies a link to
 * that clause and says so. Without the script the binder still reads, links and prints from the
 * browser's own menu.
 */

const SAY_FOR = 2200;

/** Scrolls the rail sideways so the raised tab sits in the middle, without moving the page. */
const centreTab = (): void => {
  const rail = document.querySelector<HTMLElement>("[data-binder]");
  const tab = rail?.querySelector<HTMLElement>("[aria-current='page']");
  if (rail && tab) {
    rail.scrollLeft = tab.offsetLeft - (rail.clientWidth - tab.offsetWidth) / 2;
  }
};

const startPrint = (): void => {
  for (const button of document.querySelectorAll<HTMLButtonElement>(
    "[data-print]"
  )) {
    button.hidden = false;
    button.addEventListener("click", () => {
      window.print();
    });
  }
};

/** Gives each clause heading a button that copies the address of that clause. */
const startClauseLinks = (): void => {
  const status = document.querySelector<HTMLElement>("[data-binder-status]");
  const icon = document.querySelector<SVGElement>(
    ".legal-action [data-glyph='link']"
  );
  for (const heading of document.querySelectorAll<HTMLHeadingElement>(
    ".clause > h2[id]"
  )) {
    const number =
      heading.querySelector(".clause-number")?.textContent ?? heading.id;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "clause-link";
    button.setAttribute("aria-label", `Copy a link to section ${number}`);
    if (icon) {
      button.append(icon.cloneNode(true));
    }
    let timer = 0;
    const copy = async (): Promise<void> => {
      const address = `${window.location.origin}${window.location.pathname}#${heading.id}`;
      await navigator.clipboard.writeText(address);
      window.history.replaceState(null, "", `#${heading.id}`);
      button.dataset.copied = "";
      if (status) {
        status.textContent = `Link to section ${number} copied`;
      }
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        delete button.dataset.copied;
      }, SAY_FOR);
    };
    button.addEventListener("click", () => {
      void copy();
    });
    heading.append(button);
  }
};

/** Starts the binder's behaviour, on a page that has one. */
export const startBinder = (): void => {
  if (!document.querySelector("[data-binder]")) {
    return;
  }
  centreTab();
  startPrint();
  startClauseLinks();
};
