/**
 * The header: it settles onto a frosted bar once the page moves, steps aside while the visitor
 * reads downwards and returns as soon as they scroll back up. Its menus are native popovers, so
 * they open, close on Escape and dismiss on an outside click without this script; here they
 * also open on hover for a mouse, and their triggers report whether they are expanded.
 */

const HIDE_AFTER = 160;

const trackScroll = (header: HTMLElement): void => {
  let previous = window.scrollY;
  let frame = 0;
  const update = (): void => {
    frame = 0;
    const current = window.scrollY;
    const menuOpen = document.querySelector(":popover-open") !== null;
    header.classList.toggle("is-scrolled", current > 8);
    if (!menuOpen && current > HIDE_AFTER && current > previous + 4) {
      header.classList.add("is-hidden");
    } else if (current < previous - 4 || current <= HIDE_AFTER) {
      header.classList.remove("is-hidden");
    }
    previous = current;
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
};

const linkMenus = (): void => {
  const hover = window.matchMedia("(hover: hover) and (pointer: fine)");
  for (const trigger of document.querySelectorAll<HTMLButtonElement>(
    "[data-menu-trigger]"
  )) {
    const id = trigger.getAttribute("popovertarget");
    const menu = id
      ? document.querySelector<HTMLElement>(`#${CSS.escape(id)}`)
      : null;
    if (!menu) {
      continue;
    }
    menu.addEventListener("toggle", (event) => {
      const open = event instanceof ToggleEvent && event.newState === "open";
      trigger.setAttribute("aria-expanded", String(open));
    });
    for (const link of menu.querySelectorAll("a")) {
      link.addEventListener("click", () => menu.hidePopover());
    }
    if (!Object.hasOwn(trigger.dataset, "menuHover")) {
      continue;
    }
    let closing = 0;
    const open = (): void => {
      window.clearTimeout(closing);
      if (hover.matches && !menu.matches(":popover-open")) {
        menu.showPopover();
      }
    };
    const close = (): void => {
      window.clearTimeout(closing);
      closing = window.setTimeout(() => {
        if (
          hover.matches &&
          !menu.matches(":hover") &&
          !trigger.matches(":hover")
        ) {
          menu.hidePopover();
        }
      }, 180);
    };
    trigger.addEventListener("pointerenter", open);
    trigger.addEventListener("pointerleave", close);
    menu.addEventListener("pointerenter", open);
    menu.addEventListener("pointerleave", close);
  }
};

/** Starts the header's behaviour. */
export const startHeader = (): void => {
  const header = document.querySelector<HTMLElement>("[data-header]");
  if (header) {
    trackScroll(header);
  }
  linkMenus();
};
