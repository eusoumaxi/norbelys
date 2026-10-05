/**
 * Copy buttons: a command or the code sample on show goes to the clipboard, and the button says
 * so with a check that draws itself. Without this script, or without a clipboard, the buttons
 * stay hidden and the text is there to select.
 *
 * - `data-copy="…"` copies its own text.
 * - `data-copy-target="<id>"` copies the code on show inside that element: the visible panel of a
 *   tabbed sample, or its only one.
 */

const SHOWN_FOR = 1800;

const textOf = (button: HTMLButtonElement): string => {
  const own = button.dataset.copy;
  if (own) {
    return own;
  }
  const id = button.dataset.copyTarget;
  const host = id ? document.querySelector(`#${CSS.escape(id)}`) : null;
  const panels = host
    ? [...host.querySelectorAll<HTMLElement>("[data-panel]")]
    : [];
  const shown = panels.find((panel) => !panel.hidden) ?? panels[0];
  return shown?.querySelector("pre")?.textContent?.trimEnd() ?? "";
};

/** Wires every copy button on the page. */
export const startCopy = (): void => {
  const buttons = document.querySelectorAll<HTMLButtonElement>(
    "[data-copy], [data-copy-target]"
  );
  if (buttons.length === 0 || !navigator.clipboard) {
    return;
  }
  const status = document.querySelector<HTMLElement>("[data-copy-status]");
  for (const button of buttons) {
    button.hidden = false;
    let timer = 0;
    button.addEventListener("click", async () => {
      window.clearTimeout(timer);
      try {
        await navigator.clipboard.writeText(textOf(button));
        button.dataset.state = "copied";
        if (status) {
          status.textContent = "Copied to the clipboard.";
        }
      } catch {
        delete button.dataset.state;
        if (status) {
          status.textContent =
            "Couldn’t copy. Select the text and copy it instead.";
        }
      }
      timer = window.setTimeout(() => {
        delete button.dataset.state;
        if (status) {
          status.textContent = "";
        }
      }, SHOWN_FOR);
    });
  }
};
