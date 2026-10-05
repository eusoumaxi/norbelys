/**
 * The growth page's mixing desk (components/teams/growth-desk.astro): four channel strips, one
 * per version of a step, and a master section. A fader is a version's weight, and moving one
 * sends the step by weight, every strip's share following it. The split switch and the "winner
 * by" keys are the step's real settings. Play runs a simulated round by the product's own rules:
 * each person goes to the version furthest below its share (a tie to the first), a test that
 * picks a winner stays even until it ends, and the best rate per email sent wins, a tie going to
 * the version listed first. The rates behind a round are made up, a new draw every time.
 *
 * While the desk waits for Play its meters show each version's level, flickering gently while
 * the desk is on screen. Under reduced motion nothing glides or flickers and a round's result
 * shows at once; without the script the desk shows its last round.
 */

type Split = "balanced" | "weighted" | "automatic";
type Objective = "replies" | "clicks" | "opens";
type Phase = "ready" | "running" | "done";

/** The people in the step, and how many each version gets while a test runs. */
const PEOPLE = 2000;
const TEST = 250;
/** The test's window, in days (the product's default), and the meters' height. */
const WINDOW = 7;
const SEGMENTS = 12;
/** A level the faders sit at when no weight applies, and the weights' range. */
const LEVEL = 50;
const LOWEST = 1;
const HIGHEST = 100;

/** The simulated rates per email sent, and the rate one meter segment stands for. */
const RATES: Record<
  Objective,
  { readonly low: number; readonly high: number; readonly unit: number }
> = {
  clicks: { high: 0.08, low: 0.012, unit: 0.0075 },
  opens: { high: 0.52, low: 0.1, unit: 0.05 },
  replies: { high: 0.054, low: 0.008, unit: 0.005 },
};
const WORDS: Record<
  Objective,
  { readonly label: string; readonly many: string; readonly one: string }
> = {
  clicks: { label: "Clicks", many: "clicks", one: "click" },
  opens: { label: "Opens", many: "opens", one: "open" },
  replies: { label: "Replies", many: "replies", one: "reply" },
};
/** How long each part of a round takes, in milliseconds. */
const TIME = {
  decide: 900,
  glide: 650,
  intro: 950,
  rollout: 1500,
  send: 2600,
} as const;

interface Strip {
  readonly root: HTMLElement;
  readonly letter: string;
  readonly fader: HTMLInputElement;
  readonly pot: SVGElement;
  readonly share: HTMLElement;
  readonly note: HTMLElement;
  readonly sent: HTMLElement;
  readonly hits: HTMLElement;
  readonly metric: HTMLElement;
  readonly segments: readonly HTMLElement[];
}

/** One part of a round: what it sets up, what it draws as it goes (0 to 1), how it ends. */
interface Part {
  readonly duration: number;
  readonly begin?: () => void;
  readonly draw?: (progress: number) => void;
  readonly end?: () => void;
}

const isSplit = (value: string): value is Split =>
  value === "balanced" || value === "weighted" || value === "automatic";
const isObjective = (value: string): value is Objective =>
  value === "replies" || value === "clicks" || value === "opens";

const clamp = (value: number, low = 0, high = 1): number =>
  Math.min(high, Math.max(low, value));
const easeOut = (value: number): number => 1 - (1 - value) ** 3;
const easeInOut = (value: number): number =>
  value < 0.5 ? 4 * value ** 3 : 1 - (-2 * value + 2) ** 3 / 2;
const format = (value: number): string =>
  Math.round(value).toLocaleString("en-US");
const list = (items: readonly string[]): string =>
  items.length < 2
    ? items.join("")
    : `${items.slice(0, -1).join(", ")} and ${items.at(-1) ?? ""}`;

/** The next person's version: the one furthest below its share, a tie going to the first. */
const choose = (
  assigned: readonly number[],
  weights: readonly number[]
): number => {
  let best = 0;
  for (let index = 1; index < assigned.length; index += 1) {
    // (assigned + 1) / weight, compared by cross-multiplication, as the server compares them.
    const mine = ((assigned[index] ?? 0) + 1) * (weights[best] ?? 1);
    const theirs = ((assigned[best] ?? 0) + 1) * (weights[index] ?? 1);
    if (mine < theirs) {
      best = index;
    }
  }
  return best;
};

/** The best rate per email sent; a tie goes to the version listed first. */
const leaderOf = (hits: readonly number[], sent: readonly number[]): number => {
  let best = 0;
  for (let index = 1; index < hits.length; index += 1) {
    if (
      (hits[index] ?? 0) * (sent[best] ?? 0) >
      (hits[best] ?? 0) * (sent[index] ?? 0)
    ) {
      best = index;
    }
  }
  return best;
};

/** Each version's share of the step, in whole percents that add up to 100. */
const sharesOf = (weights: readonly number[]): number[] => {
  const total = weights.reduce((sum, weight) => sum + weight, 0);
  const exact = weights.map((weight) =>
    total > 0 ? (weight * 100) / total : 0
  );
  const shares = exact.map(Math.floor);
  let left = 100 - shares.reduce((sum, share) => sum + share, 0);
  const order = exact
    .map((value, index) => ({ index, rest: value - Math.floor(value) }))
    .toSorted((a, b) => b.rest - a.rest);
  for (const { index } of order) {
    if (left <= 0) {
      break;
    }
    shares[index] = (shares[index] ?? 0) + 1;
    left -= 1;
  }
  return shares;
};

/** Four made-up rates, spread apart so a round has a clear order, in a new order every time. */
const drawRates = (objective: Objective, count: number): number[] => {
  const { high, low } = RATES[objective];
  const slots = Array.from(
    { length: count },
    (_, index) => (index + 0.5) / count
  );
  for (let index = slots.length - 1; index > 0; index -= 1) {
    const other = Math.floor(Math.random() * (index + 1));
    [slots[index], slots[other]] = [slots[other] ?? 0, slots[index] ?? 0];
  }
  return slots.map(
    (slot) => low + clamp(slot + (Math.random() - 0.5) * 0.18) * (high - low)
  );
};

/** A channel strip's parts, or nothing if one is missing. */
const readStrip = (root: HTMLElement): Strip | undefined => {
  const fader = root.querySelector<HTMLInputElement>("[data-strip-fader]");
  const pot = root.querySelector<SVGElement>("[data-strip-pot]");
  const share = root.querySelector<HTMLElement>("[data-strip-share]");
  const note = root.querySelector<HTMLElement>("[data-strip-note]");
  const sent = root.querySelector<HTMLElement>("[data-strip-sent]");
  const hits = root.querySelector<HTMLElement>("[data-strip-hits]");
  const metric = root.querySelector<HTMLElement>("[data-strip-metric]");
  if (!(fader && pot && share && note && sent && hits && metric)) {
    return undefined;
  }
  return {
    fader,
    hits,
    letter: root.dataset.letter ?? "",
    metric,
    note,
    pot,
    root,
    segments: [...root.querySelectorAll<HTMLElement>(".strip-meter i")],
    sent,
    share,
  };
};

/** The value of the checked input in a group of radios. */
const checked = (inputs: readonly HTMLInputElement[]): string =>
  inputs.find((input) => input.checked)?.value ?? "";

/** Lights a meter to `lit` segments; a signal level is grey, replies are pink. */
const paintMeter = (
  strip: Strip,
  lit: number,
  kind: "hits" | "signal"
): void => {
  const count = Math.round(clamp(lit, 0, SEGMENTS));
  for (const [index, segment] of strip.segments.entries()) {
    const on = index < count;
    segment.classList.toggle("is-lit", on && kind === "hits");
    segment.classList.toggle("is-signal", on && kind === "signal");
    segment.classList.toggle("is-peak", kind === "hits" && index === count - 1);
  }
};

/** What Play says in each phase of a round. */
const PLAY_LABELS: Record<Phase, string> = {
  done: "Play again",
  ready: "Play a round",
  running: "Skip to the result",
};

/** Starts the desk, if the page has one. */
export const startMixer = (): void => {
  const desk = document.querySelector<HTMLElement>("[data-mixer]");
  if (!desk) {
    return;
  }
  const status = desk.querySelector<HTMLElement>("[data-desk-status]");
  const clock = desk.querySelector<HTMLElement>("[data-desk-clock]");
  const bar = desk.querySelector<HTMLElement>("[data-desk-progress]");
  const play = desk.querySelector<HTMLButtonElement>("[data-desk-play]");
  const playLabel = desk.querySelector<HTMLElement>("[data-desk-play-label]");
  if (!(status && clock && bar && play && playLabel)) {
    return;
  }
  const found = [...desk.querySelectorAll<HTMLElement>("[data-strip]")].map(
    readStrip
  );
  const strips = found.filter((strip): strip is Strip => strip !== undefined);
  if (strips.length !== found.length) {
    return;
  }
  const splitInputs = [
    ...desk.querySelectorAll<HTMLInputElement>('input[name="growth-split"]'),
  ];
  const objectiveInputs = [
    ...desk.querySelectorAll<HTMLInputElement>(
      'input[name="growth-objective"]'
    ),
  ];
  const motion = document.documentElement.classList.contains("motion");

  const startSplit = checked(splitInputs);
  const startObjective = checked(objectiveInputs);
  let split: Split = isSplit(startSplit) ? startSplit : "automatic";
  let objective: Objective = isObjective(startObjective)
    ? startObjective
    : "replies";
  let phase: Phase = "done";
  /** The weights "By weight" sends with, kept while another split holds the faders level. */
  let weights = strips.map((strip) =>
    Number(strip.root.dataset.weight ?? LEVEL)
  );
  /** The named winner, while its step sends it to everyone left. */
  let winner: number | undefined = strips.findIndex((strip) =>
    strip.root.classList.contains("is-winner")
  );
  if (winner < 0) {
    winner = undefined;
  }

  const values = (): number[] =>
    strips.map((strip) => Number(strip.fader.value));
  const setValues = (next: readonly number[]): void => {
    for (const [index, strip] of strips.entries()) {
      strip.fader.value = String(
        Math.round(clamp(next[index] ?? LEVEL, LOWEST, HIGHEST))
      );
    }
  };

  /** Each version's share of the step's people, as the split gives it right now. */
  const currentShares = (): number[] => {
    if (winner !== undefined) {
      return strips.map((_, index) => (index === winner ? 100 : 0));
    }
    return sharesOf(split === "weighted" ? values() : strips.map(() => 1));
  };

  /** Writes every strip's share, its pot, its note and what a screen reader hears of its fader. */
  const paintShares = (): void => {
    const shares = currentShares();
    for (const [index, strip] of strips.entries()) {
      const share = shares[index] ?? 0;
      strip.share.textContent = `${share}%`;
      strip.pot.style.setProperty(
        "--turn",
        `${Math.round(-135 + share * 2.7)}deg`
      );
      let note = "even split";
      let spoken = `${share}% of people, an even split`;
      if (winner !== undefined) {
        note = index === winner ? "winner" : "tested";
        spoken =
          index === winner
            ? "the winner, 100% of the people left"
            : "tested, no more people";
      } else if (split === "weighted") {
        note = `weight ${strip.fader.value}`;
        spoken = `weight ${strip.fader.value}, ${share}% of people`;
      } else if (split === "automatic") {
        note = phase === "running" ? "testing" : "even test";
        spoken = `${share}% of people while the test runs`;
      }
      strip.note.textContent = note;
      strip.fader.setAttribute("aria-valuetext", spoken);
    }
  };

  const setPhase = (next: Phase): void => {
    phase = next;
    desk.dataset.phase = next;
    playLabel.textContent = PLAY_LABELS[next];
  };
  const setProgress = (value: number): void => {
    bar.style.setProperty("--progress", clamp(value).toFixed(3));
  };
  const say = (words: string): void => {
    status.textContent = words;
  };
  const setLamps = (
    lead: number | undefined,
    won: number | undefined
  ): void => {
    for (const [index, strip] of strips.entries()) {
      strip.root.classList.toggle("is-winner", index === won);
      strip.root.classList.toggle(
        "is-leading",
        index === lead && won === undefined
      );
    }
  };

  // ——— The animation runner: parts played one after another, or all at once. ———
  let parts: Part[] = [];
  let partStart = 0;
  let begun = false;
  let frame = 0;
  const finishAll = (): void => {
    window.cancelAnimationFrame(frame);
    frame = 0;
    const left = parts;
    parts = [];
    for (const [index, part] of left.entries()) {
      if (index > 0 || !begun) {
        part.begin?.();
      }
      part.draw?.(1);
      part.end?.();
    }
    begun = false;
  };
  const tick = (time: number): void => {
    frame = 0;
    const [part] = parts;
    if (!part) {
      return;
    }
    if (!begun) {
      begun = true;
      partStart = time;
      part.begin?.();
    }
    const elapsed = clamp((time - partStart) / Math.max(1, part.duration));
    part.draw?.(elapsed);
    if (elapsed >= 1) {
      part.end?.();
      parts.shift();
      begun = false;
    }
    if (parts.length > 0) {
      frame = window.requestAnimationFrame(tick);
    }
  };
  const run = (next: readonly Part[]): void => {
    finishAll();
    parts = [...next];
    if (motion) {
      frame = window.requestAnimationFrame(tick);
    } else {
      finishAll();
    }
  };

  /** A part that moves the faders from wherever they are to `targets`. */
  const glideTo = (
    targets: readonly number[],
    duration: number = TIME.glide,
    stagger = 0
  ): Part => {
    let from: number[] = [];
    return {
      begin: () => {
        from = values();
      },
      draw: (progress) => {
        const span = Math.max(1, duration);
        const last = (targets.length - 1) * stagger;
        setValues(
          targets.map((target, index) => {
            const local = clamp(
              (progress * span - index * stagger) / Math.max(1, span - last)
            );
            const start = from[index] ?? target;
            return start + (target - start) * easeInOut(local);
          })
        );
        paintShares();
      },
      duration,
    };
  };

  /** Where the faders sit for the split: level unless the step sends by weight. */
  const levels = (): number[] => {
    if (winner !== undefined) {
      return strips.map((_, index) => (index === winner ? HIGHEST : LOWEST));
    }
    return split === "weighted" ? weights : strips.map(() => LEVEL);
  };

  // ——— Waiting for Play: each meter shows its version's level, and flickers while in view. ———
  let awake = false;
  let flicker = 0;
  const signal = (jitter: boolean): void => {
    const shares = currentShares();
    for (const [index, strip] of strips.entries()) {
      const base = Math.round(((shares[index] ?? 0) / 100) * 10);
      const noise = jitter && base > 0 ? Math.round(Math.random() * 2 - 1) : 0;
      paintMeter(strip, Math.max(base > 0 ? 1 : 0, base + noise), "signal");
    }
  };
  const stopFlicker = (): void => {
    window.clearTimeout(flicker);
    flicker = 0;
  };
  const startFlicker = (): void => {
    stopFlicker();
    if (!(motion && awake && phase === "ready")) {
      return;
    }
    signal(true);
    flicker = window.setTimeout(startFlicker, 170 + Math.random() * 140);
  };

  const ready = (words: string): void => {
    setPhase("ready");
    setLamps(undefined, undefined);
    setProgress(0);
    clock.textContent = `${format(PEOPLE)} people`;
    for (const strip of strips) {
      strip.sent.textContent = "–";
      strip.hits.textContent = "–";
    }
    say(words);
    paintShares();
    signal(false);
    startFlicker();
  };

  const describe = (): string => {
    if (split === "balanced") {
      return "Evenly: every version goes to as many people. Press Play.";
    }
    if (split === "weighted") {
      return "By weight: each version gets its weight’s share of the people. Press Play.";
    }
    const tracking =
      objective === "replies"
        ? ""
        : ` ${WORDS[objective].label} count only once you turn tracking on.`;
    return `Pick a winner: an even test, then the best by ${WORDS[objective].many} goes to everyone left.${tracking} Press Play.`;
  };

  // ——— Settings. ———
  const changeSplit = (next: Split): void => {
    finishAll();
    if (split === "weighted" && next !== "weighted") {
      weights = values();
    }
    split = next;
    desk.dataset.split = next;
    winner = undefined;
    for (const input of splitInputs) {
      input.checked = input.value === next;
    }
    ready(describe());
    run([glideTo(levels())]);
  };
  const changeObjective = (next: Objective): void => {
    objective = next;
    desk.dataset.objective = next;
    for (const strip of strips) {
      strip.metric.textContent = WORDS[next].label;
    }
    if (split === "automatic") {
      finishAll();
      winner = undefined;
      ready(describe());
      run([glideTo(levels())]);
    } else {
      changeSplit("automatic");
    }
  };
  for (const input of splitInputs) {
    input.addEventListener("change", () => {
      if (input.checked && isSplit(input.value)) {
        changeSplit(input.value);
      }
    });
  }
  for (const input of objectiveInputs) {
    input.addEventListener("change", () => {
      if (input.checked && isObjective(input.value)) {
        changeObjective(input.value);
      }
    });
  }

  // A fader moved by hand: the step now sends by weight, starting from what the faders show.
  const moveFader = (strip: Strip): void => {
    const moved = Number(strip.fader.value);
    if (phase === "running") {
      finishAll();
      strip.fader.value = String(moved);
    }
    if (split !== "weighted" || winner !== undefined || phase !== "ready") {
      weights = values();
      split = "weighted";
      desk.dataset.split = "weighted";
      winner = undefined;
      for (const input of splitInputs) {
        input.checked = input.value === "weighted";
      }
      ready(describe());
    }
    weights = values();
    paintShares();
    signal(false);
  };
  for (const strip of strips) {
    strip.fader.addEventListener("input", () => moveFader(strip));
  }

  // ——— A round. ———
  const round = (): Part[] => {
    // A new round starts from the split itself: no winner yet, the faders level unless weighted.
    winner = undefined;
    const testing = split === "automatic";
    const people = testing ? TEST * strips.length : PEOPLE;
    const sendWeights = split === "weighted" ? values() : strips.map(() => 1);
    const order: number[] = [];
    const assigned = strips.map(() => 0);
    for (let person = 0; person < people; person += 1) {
      const index = choose(assigned, sendWeights);
      assigned[index] = (assigned[index] ?? 0) + 1;
      order.push(index);
    }
    const rates = drawRates(objective, strips.length);
    const hits = assigned.map((count, index) =>
      Math.round(count * (rates[index] ?? 0))
    );
    const lead = leaderOf(hits, assigned);
    const { unit } = RATES[objective];
    const lit = hits.map(
      (count, index) => count / Math.max(1, assigned[index] ?? 1) / unit
    );
    const tied = strips
      .map((_, index) => index)
      .filter(
        (index) =>
          index !== lead &&
          (hits[index] ?? 0) * (assigned[lead] ?? 0) ===
            (hits[lead] ?? 0) * (assigned[index] ?? 0)
      );
    const word = WORDS[objective];
    const leadStrip = strips[lead];
    const leadLetter = leadStrip?.letter ?? "";
    const leadHits = hits[lead] ?? 0;
    const leadSent = assigned[lead] ?? 0;
    const tieNote =
      tied.length > 0
        ? ` ${list([leadLetter, ...tied.map((index) => strips[index]?.letter ?? "")])} tied, so ${leadLetter} wins as the version listed first.`
        : "";
    const rest = PEOPLE - people;

    const counted = strips.map(() => 0);
    let done = 0;
    const sendPart: Part = {
      begin: () => {
        setPhase("running");
        winner = undefined;
        setLamps(undefined, undefined);
        stopFlicker();
        for (const strip of strips) {
          paintMeter(strip, 0, "hits");
        }
        if (testing) {
          say(
            `Testing evenly: ${format(TEST)} people per version, over a ${WINDOW}-day window.`
          );
        } else if (split === "weighted") {
          say(
            `Sending by weight: ${list(assigned.map(format))} of ${format(PEOPLE)} people.`
          );
        } else {
          say(
            `Sending evenly: ${format(people / strips.length)} people per version.`
          );
        }
        paintShares();
      },
      draw: (progress) => {
        const target = Math.round(people * progress);
        while (done < target) {
          const index = order[done] ?? 0;
          counted[index] = (counted[index] ?? 0) + 1;
          done += 1;
        }
        // Replies trail the sends: they arrive over the days after each email goes out.
        const arrived = clamp((progress - 0.12) / 0.88) ** 1.2;
        for (const [index, strip] of strips.entries()) {
          strip.sent.textContent = format(counted[index] ?? 0);
          strip.hits.textContent = format((hits[index] ?? 0) * arrived);
          paintMeter(strip, (lit[index] ?? 0) * arrived, "hits");
        }
        setProgress(testing ? progress * 0.7 : progress);
        clock.textContent = testing
          ? `Day ${Math.min(WINDOW, 1 + Math.floor(progress * WINDOW))} of ${WINDOW}`
          : `${format(done)} of ${format(PEOPLE)} sent`;
      },
      duration: TIME.send,
    };

    if (!testing) {
      return [
        glideTo(
          sendWeights.map((weight) => (split === "weighted" ? weight : LEVEL))
        ),
        sendPart,
        {
          duration: 0,
          end: () => {
            setLamps(lead, undefined);
            setPhase("done");
            setProgress(1);
            clock.textContent = `${format(PEOPLE)} people`;
            const keeps = split === "weighted" ? "By weight" : "Evenly";
            say(
              `Done. ${leadLetter} got the most ${word.many} per email: ${format(leadHits)} from ${format(leadSent)}.${tieNote} ${keeps}, every version keeps its share.`
            );
          },
        },
      ];
    }

    // The winner takes the mix: its fader all the way up, the others down, the rest of the step its.
    const takeover = glideTo(
      strips.map((_, index) => (index === lead ? HIGHEST : LOWEST)),
      TIME.rollout
    );
    return [
      glideTo(strips.map(() => LEVEL)),
      sendPart,
      {
        begin: () => {
          winner = lead;
          setLamps(undefined, lead);
          clock.textContent = `Day ${WINDOW} of ${WINDOW}`;
          say(
            `${leadLetter} wins on ${word.many}: ${format(leadHits)} from ${format(leadSent)}.${tieNote}`
          );
          paintShares();
        },
        draw: (progress) => setProgress(0.7 + progress * 0.05),
        duration: TIME.decide,
      },
      {
        begin: () => takeover.begin?.(),
        draw: (progress) => {
          takeover.draw?.(progress);
          leadStrip?.sent.replaceChildren(
            format(leadSent + rest * easeOut(progress))
          );
          clock.textContent = `${format(rest * (1 - easeOut(progress)))} left for ${leadLetter}`;
          setProgress(0.75 + progress * 0.25);
        },
        duration: TIME.rollout,
        end: () => {
          setPhase("done");
          clock.textContent = `${format(PEOPLE)} people`;
          say(
            `${leadLetter} won on ${word.many}, ${format(leadHits)} from ${format(leadSent)}.${tieNote} The ${format(rest)} people left get ${leadLetter}.`
          );
        },
      },
    ];
  };

  play.addEventListener("click", () => {
    if (phase === "running") {
      finishAll();
      return;
    }
    run(round());
  });

  // ——— On screen or off: the meters flicker only while someone can see them. ———
  new IntersectionObserver((entries) => {
    const visible = entries.some((entry) => entry.isIntersecting);
    awake = visible && document.visibilityState === "visible";
    desk.classList.toggle("is-awake", awake);
    if (!visible && phase === "running") {
      finishAll();
    }
    if (awake) {
      startFlicker();
    } else {
      stopFlicker();
    }
  }).observe(desk);
  document.addEventListener("visibilitychange", () => {
    awake = document.visibilityState === "visible" && awake;
    if (!awake) {
      stopFlicker();
    }
  });

  // ——— Power on: the faders and meters come up to the last round, like a motorised desk. ———
  paintShares();
  if (motion) {
    const last = values();
    const lit = strips.map(
      (strip) =>
        strip.segments.filter((segment) => segment.classList.contains("is-lit"))
          .length
    );
    setValues(strips.map(() => LOWEST));
    for (const strip of strips) {
      paintMeter(strip, 0, "hits");
    }
    const rise = glideTo(last, TIME.intro, 110);
    window.setTimeout(
      () =>
        run([
          {
            ...rise,
            draw: (progress) => {
              rise.draw?.(progress);
              for (const [index, strip] of strips.entries()) {
                paintMeter(
                  strip,
                  (lit[index] ?? 0) * easeOut(progress),
                  "hits"
                );
              }
            },
          },
        ]),
      350
    );
  }
};
