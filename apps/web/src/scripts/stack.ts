/**
 * The agency page's stack of client reports (components/teams/agency-stack.astro). Tapping the
 * top sheet, or Next client, deals it: the sheet lifts, flies up and over the stack and drops in
 * at the back, while the next client's report comes forward and draws its week. A sheet's tab
 * brings that client to the front by dealing the sheets above it, one after another. The stack
 * deals itself twice while it's in view, then waits for the visitor.
 *
 * Under reduced motion nothing flies: the order simply changes.
 */

/** A sheet's flight off the top of the stack, and its fall back in at the back, in ms. */
const FLIGHT = 680;
const RETURN = 900;
/** The beat between sheets when a tab deals several of them. */
const RIFFLE = 190;
/** When the stack deals itself, after it first comes into view. */
const AUTO_DEALS = [2900, 6600] as const;
/** Where a dealt sheet flies to: up past the top of the stack, turning as it goes. */
const AWAY = "translate(10%, -122%) rotate(-9deg)";
const LIFT = "translateY(-3%) rotate(-0.8deg)";

/** A sheet's client, as its report names it. */
const nameOf = (sheet: HTMLElement): string =>
  sheet.querySelector(".report-client")?.textContent?.trim() ?? "";

/** Starts the stack, if the page has one. */
export const startStack = (): void => {
  const hero = document.querySelector<HTMLElement>("[data-stack]");
  const stack = hero?.querySelector<HTMLElement>(".stack");
  const deal = hero?.querySelector<HTMLButtonElement>("[data-stack-deal]");
  const next = hero?.querySelector<HTMLButtonElement>("[data-stack-next]");
  const status = hero?.querySelector<HTMLElement>("[data-stack-status]");
  const sheets = [
    ...(hero?.querySelectorAll<HTMLElement>("[data-stack-sheet]") ?? []),
  ];
  if (!(hero && stack && deal && next && status) || sheets.length < 2) {
    return;
  }
  const motion = document.documentElement.classList.contains("motion");

  /** The clients' sheets, front first. */
  let order = sheets.map((_, index) => index);
  const flying = new Set<HTMLElement>();

  /** Writes the order onto the sheets: each one's place, and which tab is pressed. */
  const place = (): void => {
    for (const [slot, index] of order.entries()) {
      const sheet = sheets[index];
      if (!sheet) {
        continue;
      }
      const front = slot === 0;
      sheet.style.setProperty("--slot", String(slot));
      if (front) {
        sheet.classList.add("is-front");
      } else if (
        !flying.has(sheet) &&
        !sheet.classList.contains("is-returning")
      ) {
        sheet.classList.remove("is-front");
      }
      sheet
        .querySelector("[data-stack-tab]")
        ?.setAttribute("aria-pressed", String(front));
      const page = sheet.querySelector(".report-page");
      if (front) {
        page?.removeAttribute("aria-hidden");
      } else {
        page?.setAttribute("aria-hidden", "true");
      }
    }
  };

  const announce = (): void => {
    const front = sheets[order[0] ?? 0];
    if (front) {
      status.textContent = `${nameOf(front)}’s report is on top.`;
    }
  };

  /** Once a sheet's flight ends, it falls in behind the others; a cancelled one just stops. */
  const land = async (sheet: HTMLElement, flight: Animation): Promise<void> => {
    try {
      await flight.finished;
    } catch {
      flying.delete(sheet);
      sheet.classList.remove("is-flying");
      return;
    }
    flight.commitStyles();
    flight.cancel();
    flying.delete(sheet);
    sheet.classList.remove("is-flying");
    sheet.classList.add("is-returning");
    // The flight's end is where the fall starts: the sheet now sits behind the others.
    void sheet.offsetWidth;
    sheet.style.removeProperty("transform");
    window.setTimeout(() => {
      sheet.classList.remove("is-returning");
      if (sheets[order[0] ?? -1] !== sheet) {
        sheet.classList.remove("is-front");
      }
    }, RETURN);
  };

  /** Sends one sheet up and over, then lets it fall in behind the others. */
  const fly = (sheet: HTMLElement): void => {
    flying.add(sheet);
    const from = getComputedStyle(sheet).transform;
    const start = from === "none" ? "" : from;
    sheet.classList.add("is-flying");
    // A quick lift off the pile, then one smooth pull up and away.
    const flight = sheet.animate(
      [
        {
          easing: "cubic-bezier(0.2, 0.8, 0.2, 1)",
          transform: start || "none",
        },
        {
          easing: "cubic-bezier(0.4, 0, 0.75, 0.55)",
          offset: 0.22,
          transform: `${start} ${LIFT}`,
        },
        { transform: AWAY },
      ],
      { duration: FLIGHT, fill: "forwards" }
    );
    void land(sheet, flight);
  };

  /** Deals the top sheet to the back. */
  const dealOne = (): void => {
    const [index] = order;
    const sheet = index === undefined ? undefined : sheets[index];
    if (index === undefined || !sheet || flying.has(sheet)) {
      return;
    }
    order = [...order.slice(1), index];
    if (motion) {
      fly(sheet);
      stack.classList.add("is-dealt");
    }
    place();
  };

  // The deals the stack makes by itself, until the visitor takes over.
  let timers: number[] = [];
  let touched = false;
  const stopAuto = (): void => {
    for (const timer of timers) {
      window.clearTimeout(timer);
    }
    timers = [];
  };
  const touch = (): void => {
    touched = true;
    stopAuto();
  };
  if (motion) {
    let started = false;
    new IntersectionObserver(
      (entries) => {
        const visible = entries.some((entry) => entry.isIntersecting);
        if (visible && !started && !touched) {
          started = true;
          timers = AUTO_DEALS.map((delay) =>
            window.setTimeout(() => {
              if (document.visibilityState === "visible") {
                dealOne();
              }
            }, delay)
          );
        } else if (!visible) {
          stopAuto();
        }
      },
      { threshold: 0.3 }
    ).observe(stack);
  }

  // A tab deals every sheet above its own, one beat apart, until its client is on top.
  let riffle = 0;
  let target = -1;
  const bringForward = (index: number): void => {
    touch();
    target = index;
    if (order[0] === target) {
      return;
    }
    if (!motion) {
      const steps = order.indexOf(target);
      order = [...order.slice(steps), ...order.slice(0, steps)];
      place();
      announce();
      return;
    }
    if (riffle !== 0) {
      return;
    }
    dealOne();
    riffle = window.setInterval(() => {
      if (order[0] === target) {
        window.clearInterval(riffle);
        riffle = 0;
        announce();
        return;
      }
      dealOne();
    }, RIFFLE);
  };

  for (const [index, sheet] of sheets.entries()) {
    sheet
      .querySelector("[data-stack-tab]")
      ?.addEventListener("click", () => bringForward(index));
  }
  const dealNext = (): void => {
    touch();
    if (riffle !== 0) {
      return;
    }
    dealOne();
    announce();
  };
  deal.addEventListener("click", dealNext);
  next.addEventListener("click", dealNext);
};
