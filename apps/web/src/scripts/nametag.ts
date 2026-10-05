/**
 * The recruiter page's name tags. The headline reads "Hello, {first_name}." and, once the tags
 * around it are stuck on, the field resolves into each of their names in turn: the name flips
 * into the headline, the underline draws under it and that tag's bands turn pink. On Jonas's turn
 * his reply lands and his next follow-up is struck. After one round the field comes home to the
 * tag under the headline, the one the visitor can write on, and the headline rests there.
 *
 * Whatever name the visitor types on that tag fills every {first_name} on the page, the
 * headline's included; clearing it brings the field back. The round pauses while the headline is
 * out of view or the tab is hidden, and stops for good once the visitor starts writing.
 *
 * Under reduced motion nothing cycles: the headline keeps its field, Jonas has already answered
 * and typing fills every field at once.
 */

/** When the round begins (once the tags are on), and how long each name holds the headline. */
const START_AFTER = 1800;
const HOLD = 2300;
/** How long after his name arrives a candidate's reply lands. */
const REPLY_AFTER = 750;
/** How long the headline's width takes to follow a new name (the stylesheet's transition). */
const RESIZE = 520;
/** The longest name the tag takes; the input's maxlength says the same. */
const LONGEST = 20;
/** The field's own key, and the face that shows what the visitor typed. */
const FIELD = "field";
const TYPED = "typed";
const FIELD_TEXT = "{first_name}";

/** A name as it may be written: single spaces, nothing at either end, no longer than the tag. */
const tidy = (value: string): string =>
  value.replaceAll(/\s+/gu, " ").trim().slice(0, LONGEST);

/** Starts the name tags, if the page has them. */
export const startNametag = (): void => {
  const hero = document.querySelector<HTMLElement>("[data-nametag]");
  const title = hero?.querySelector<HTMLElement>(".tag-title");
  const slot = hero?.querySelector<HTMLElement>("[data-nametag-slot]");
  const input = hero?.querySelector<HTMLInputElement>("[data-nametag-input]");
  const measure = hero?.querySelector<HTMLElement>("[data-nametag-measure]");
  if (!(hero && title && slot && input && measure)) {
    return;
  }
  const faces = new Map<string, HTMLElement>();
  for (const face of slot.querySelectorAll<HTMLElement>(
    "[data-nametag-face]"
  )) {
    faces.set(face.dataset.nametagFace ?? "", face);
  }
  const field = faces.get(FIELD);
  const typed = faces.get(TYPED);
  if (!(field && typed)) {
    return;
  }
  const motion = document.documentElement.classList.contains("motion");
  const tags = [
    ...document.querySelectorAll<HTMLElement>("[data-nametag-tag]"),
  ];
  const repliers = tags.filter((tag) =>
    Object.hasOwn(tag.dataset, "nametagReplies")
  );
  const fills = [
    ...document.querySelectorAll<HTMLElement>("[data-nametag-name]"),
  ];
  const hello = title.querySelector<HTMLElement>(".tag-say-hello");
  const stop =
    slot.nextElementSibling instanceof HTMLElement
      ? slot.nextElementSibling
      : undefined;

  // The round: the field, each tag's name in the order they're stuck on, and the field again.
  const round = [
    FIELD,
    ...tags
      .map((tag) => tag.dataset.nametagTag ?? FIELD)
      .filter((key) => key !== FIELD && faces.has(key)),
    FIELD,
  ];

  /** How wide the name may grow before the headline would need another line. */
  const room = (): number => {
    // A computed font size is always in pixels: "96px".
    const size = Number(getComputedStyle(title).fontSize.slice(0, -2));
    const sameLine =
      hello !== null && getComputedStyle(hello).display !== "block";
    const before = sameLine && hello ? hello.offsetWidth + size * 0.26 : 0;
    return title.clientWidth - before - (stop?.offsetWidth ?? 0) - size * 0.1;
  };

  /**
   * Shrinks a typed name that wouldn't fit, underline and all, so the headline never takes
   * another line; every other face is short enough to keep the headline's size.
   */
  const fit = (key: string, face: HTMLElement): void => {
    slot.style.fontSize = "";
    if (key !== TYPED) {
      return;
    }
    const width = face.offsetWidth;
    const space = room();
    if (width > space && width > 0) {
      slot.style.fontSize = `${(space / width).toFixed(3)}em`;
    }
  };

  /** Lights the tag whose name the headline says; the typed name belongs to the writable tag. */
  const light = (key: string): void => {
    const owner = key === TYPED ? FIELD : key;
    for (const tag of tags) {
      tag.classList.toggle("is-active", tag.dataset.nametagTag === owner);
    }
  };

  let shown = field;
  let resizing = 0;
  const settle = (face: HTMLElement): void => {
    window.setTimeout(() => {
      if (face !== shown) {
        face.classList.remove("is-leaving");
      }
    }, RESIZE);
  };

  /** Puts one face in the headline: the old one flips up and out, the new one comes in under it. */
  const show = (key: string): void => {
    const face = faces.get(key);
    if (!face) {
      return;
    }
    fit(key, face);
    const typing = slot.classList.contains("is-typing");
    if (face !== shown) {
      const from = slot.offsetWidth;
      const leaving = shown;
      leaving.classList.remove("is-shown");
      leaving.classList.add("is-leaving");
      settle(leaving);
      face.classList.remove("is-leaving");
      face.classList.add("is-shown");
      shown = face;
      // Draw the underline again, under the new name.
      slot.classList.remove("is-drawn");
      if (motion && !typing) {
        window.clearTimeout(resizing);
        slot.style.width = `${from}px`;
        // Reading the layout fixes the old width as the transition's start.
        void slot.offsetWidth;
        slot.style.width = `${face.offsetWidth}px`;
        resizing = window.setTimeout(() => {
          slot.style.width = "";
        }, RESIZE);
      }
      void slot.offsetWidth;
    }
    slot.classList.toggle("is-named", key !== FIELD);
    slot.classList.add("is-drawn");
    light(key);
  };

  // The headline's faces stack on one another from here on; the shown one sets the width.
  slot.classList.add("is-live");

  // ——— The round. ———
  let step = 0;
  let running = motion;
  let started = false;
  let visible = false;
  let timer = 0;

  const finish = (): void => {
    running = false;
    window.clearTimeout(timer);
    for (const replier of repliers) {
      replier.classList.add("is-replied");
    }
  };

  // The next turn of the round: `advance` once it exists, so the timer can call it.
  let next: () => void = finish;
  const schedule = (delay: number): void => {
    window.clearTimeout(timer);
    if (running && visible && !document.hidden) {
      timer = window.setTimeout(() => next(), delay);
    }
  };

  const advance = (): void => {
    step += 1;
    const key = round[step];
    if (key === undefined) {
      finish();
      return;
    }
    show(key);
    const replier = repliers.find((tag) => tag.dataset.nametagTag === key);
    if (replier) {
      window.setTimeout(() => replier.classList.add("is-replied"), REPLY_AFTER);
    }
    if (step >= round.length - 1) {
      finish();
      return;
    }
    schedule(HOLD);
  };
  next = advance;

  if (running) {
    new IntersectionObserver((entries) => {
      visible = entries.some((entry) => entry.isIntersecting);
      if (visible) {
        schedule(started ? HOLD : START_AFTER);
        started = true;
      } else {
        window.clearTimeout(timer);
      }
    }).observe(title);
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) {
        window.clearTimeout(timer);
      } else {
        schedule(HOLD);
      }
    });
  }

  // ——— Writing on the tag. ———
  let typingTimer = 0;
  /** Sizes the name on the tag to the tag: long names get smaller, never cut off. */
  const fitInput = (name: string): void => {
    measure.textContent = name;
    const width = measure.offsetWidth;
    const space = input.clientWidth - 6;
    const scale = width > space && width > 0 ? space / width : 1;
    input.style.setProperty("--fit", scale.toFixed(3));
  };

  const write = (): void => {
    const name = tidy(input.value);
    if (running) {
      finish();
    }
    slot.classList.add("is-typing");
    window.clearTimeout(typingTimer);
    typingTimer = window.setTimeout(
      () => slot.classList.remove("is-typing"),
      300
    );
    typed.textContent = name;
    for (const fill of fills) {
      const fallback = fill.dataset.nametagFallback;
      fill.textContent = name || fallback || FIELD_TEXT;
      fill.classList.toggle("is-field", name === "" && !fallback);
    }
    show(name === "" ? FIELD : TYPED);
    fitInput(name);
  };

  input.addEventListener("input", write);
  input.addEventListener("focus", () => {
    if (running) {
      finish();
      show(tidy(input.value) === "" ? FIELD : TYPED);
    }
  });

  // A name already in the field (the browser restored it) is written at once.
  if (input.value !== "") {
    write();
  }

  // The new size of everything once the page's own type has arrived, and after a resize.
  const refit = (): void => {
    if (shown === typed) {
      fit(TYPED, typed);
    }
    fitInput(tidy(input.value));
  };
  const refitWhenFontsArrive = async (): Promise<void> => {
    await document.fonts.ready;
    refit();
  };
  void refitWhenFontsArrive();
  window.addEventListener("resize", refit, { passive: true });

  // "Write a name tag" brings the tag fully into view and puts the pen in the visitor's hand.
  const spot = input.closest<HTMLElement>(".tag-spot");
  for (const link of document.querySelectorAll<HTMLAnchorElement>(
    "[data-nametag-go]"
  )) {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      spot?.scrollIntoView({
        behavior: motion ? "smooth" : "auto",
        block: "nearest",
      });
      input.focus({ preventScroll: true });
    });
  }
};
