/**
 * Progressive enhancement for the public homepage. The campaign example never sends or saves
 * data. Without JavaScript its three scenes remain readable; with it they become keyboard tabs.
 * On a wide, tall viewport the correspondence also follows the visitor's natural scroll.
 * Keyboard focus pauses that enhancement. Reduced motion keeps the manual, static presentation.
 */

/** Enhances the example with accessible tabs and a bounded desktop scroll narrative. */
const enhanceDemo = (root: HTMLElement): void => {
  const tabs = [...root.querySelectorAll<HTMLButtonElement>("[data-demo-tab]")];
  const panels = [...root.querySelectorAll<HTMLElement>("[data-demo-panel]")];
  const tabList = root.querySelector<HTMLElement>("[data-demo-tabs]");
  if (!tabList || tabs.length === 0) {
    return;
  }
  root.dataset.enhanced = "true";

  const select = (selected: HTMLButtonElement) => {
    root.dataset.scene = selected.dataset.demoTab;
    for (const tab of tabs) {
      const active = tab === selected;
      tab.setAttribute("aria-selected", String(active));
      tab.tabIndex = active ? 0 : -1;
    }
    for (const panel of panels) {
      panel.hidden = panel.dataset.demoPanel !== selected.dataset.demoTab;
    }
  };

  tabList.setAttribute("role", "tablist");
  const narrow = window.matchMedia("(max-width: 760px)");
  const orient = () =>
    tabList.setAttribute(
      "aria-orientation",
      narrow.matches ? "horizontal" : "vertical"
    );
  orient();
  narrow.addEventListener("change", orient);
  for (const [index, tab] of tabs.entries()) {
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-controls", `panel-${tab.dataset.demoTab}`);
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (event) => {
      let next = index;
      const forward = narrow.matches ? "ArrowRight" : "ArrowDown";
      const backward = narrow.matches ? "ArrowLeft" : "ArrowUp";
      switch (event.key) {
        case forward: {
          next = (index + 1) % tabs.length;
          break;
        }
        case backward: {
          next = (index + tabs.length - 1) % tabs.length;
          break;
        }
        case "Home": {
          next = 0;
          break;
        }
        case "End": {
          next = tabs.length - 1;
          break;
        }
        default: {
          return;
        }
      }
      event.preventDefault();
      const target = tabs.at(next);
      if (target) {
        select(target);
        target.focus();
      }
    });
  }
  for (const panel of panels) {
    panel.setAttribute("role", "tabpanel");
    panel.setAttribute("aria-labelledby", `tab-${panel.dataset.demoPanel}`);
    panel.tabIndex = 0;
  }
  const first = tabs.at(0);
  if (first) {
    select(first);
  }
  tabList.hidden = false;

  const story = window.matchMedia(
    "(min-width: 1100px) and (min-height: 700px) and (prefers-reduced-motion: no-preference)"
  );
  const workspace = root.querySelector<HTMLElement>(".demo-workspace");
  if (workspace) {
    const measure = () => {
      root.style.setProperty(
        "--story-workspace-height",
        `${workspace.getBoundingClientRect().height}px`
      );
    };
    measure();
    new ResizeObserver(measure).observe(workspace);
  }
  const setStory = () => {
    root.classList.toggle("scroll-story", story.matches);
  };
  setStory();
  story.addEventListener("change", setStory);
  let scheduled = false;
  window.addEventListener(
    "scroll",
    () => {
      if (
        !story.matches ||
        scheduled ||
        root.contains(document.activeElement)
      ) {
        return;
      }
      scheduled = true;
      requestAnimationFrame(() => {
        scheduled = false;
        const bounds = root.getBoundingClientRect();
        const styles = getComputedStyle(root);
        const stickyTop = Number(
          getComputedStyle(workspace ?? root).top.replace("px", "")
        );
        const travel =
          bounds.height -
          (workspace?.getBoundingClientRect().height ?? 0) -
          Number(styles.paddingTop.replace("px", "")) -
          Number(styles.paddingBottom.replace("px", "")) -
          Number(styles.borderTopWidth.replace("px", "")) -
          Number(styles.borderBottomWidth.replace("px", ""));
        if (bounds.top > stickyTop || bounds.bottom < stickyTop) {
          return;
        }
        const progress = Math.max(
          0,
          Math.min(1, (stickyTop - bounds.top) / Math.max(1, travel))
        );
        const current = tabs.at(
          Math.min(tabs.length - 1, Math.floor(progress * tabs.length))
        );
        if (current && current.getAttribute("aria-selected") !== "true") {
          select(current);
        }
      });
    },
    { passive: true }
  );
};

for (const demo of document.querySelectorAll<HTMLElement>("[data-demo]")) {
  enhanceDemo(demo);
}

for (const menu of document.querySelectorAll<HTMLDetailsElement>(
  ".mobile-menu"
)) {
  menu.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && menu.open) {
      menu.open = false;
      menu.querySelector("summary")?.focus();
    }
  });
}

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
if (!reducedMotion.matches && "IntersectionObserver" in window) {
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          entry.target.classList.add("nb-draw");
          observer.unobserve(entry.target);
        }
      }
    },
    { threshold: 0.3 }
  );
  for (const drawing of document.querySelectorAll("[data-draw]")) {
    drawing.addEventListener("animationend", (event) => {
      if (
        event instanceof AnimationEvent &&
        event.animationName === "nb-ping"
      ) {
        drawing.classList.remove("nb-draw");
      }
    });
    observer.observe(drawing);
  }
}

for (const link of document.querySelectorAll<HTMLAnchorElement>(
  ".mobile-menu a"
)) {
  link.addEventListener("click", () => {
    const menu = link.closest("details");
    if (menu) {
      menu.open = false;
    }
  });
}
