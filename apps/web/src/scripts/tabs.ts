/**
 * Tabs: a row of buttons that each show one panel. Arrow keys move between tabs, as the ARIA
 * tabs pattern expects. Without this script the first panel is the one that shows. A visitor's
 * choice is announced on the tab list as a `tabs:select` event, so a demo can follow it.
 */

/** Wires every tab list on the page. */
export const startTabs = (): void => {
  for (const list of document.querySelectorAll<HTMLElement>(
    "[role='tablist']"
  )) {
    const tabs = [...list.querySelectorAll<HTMLButtonElement>("[role='tab']")];
    const panels = tabs.map((tab) => {
      const id = tab.getAttribute("aria-controls");
      return id
        ? document.querySelector<HTMLElement>(`#${CSS.escape(id)}`)
        : null;
    });
    const select = (chosen: number, focus: boolean): void => {
      for (const [index, tab] of tabs.entries()) {
        const on = index === chosen;
        tab.setAttribute("aria-selected", String(on));
        tab.tabIndex = on ? 0 : -1;
        const panel = panels[index];
        if (panel) {
          panel.hidden = !on;
        }
      }
      if (focus) {
        tabs[chosen]?.focus();
      }
    };
    const choose = (chosen: number, focus: boolean): void => {
      select(chosen, focus);
      list.dispatchEvent(new Event("tabs:select", { bubbles: true }));
    };
    for (const [index, tab] of tabs.entries()) {
      tab.addEventListener("click", () => choose(index, false));
      tab.addEventListener("keydown", (event) => {
        const step = { ArrowLeft: -1, ArrowRight: 1 }[event.key];
        if (step !== undefined) {
          event.preventDefault();
          choose((index + step + tabs.length) % tabs.length, true);
        }
      });
    }
    select(0, false);
  }
};
