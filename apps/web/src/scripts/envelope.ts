/**
 * The contact page's envelope: choosing a team addresses it (the postmark is inked again and the
 * address written in), the page's link can name a team (`/contact#security`), the copy button
 * copies the address and says so, and writing lifts the flap and presses the seal back on while
 * the mail app opens. Without the script the list still shows every address.
 */

const SAY_FOR = 2200;

/** Restarts a one-shot animation class, even if it is still running. */
const replay = (element: HTMLElement, name: string): void => {
  element.classList.remove(name);
  // Reading the layout makes the browser see the class removed before it is added back.
  void element.offsetWidth;
  element.classList.add(name);
};

/** Starts the envelope, on a page that has one. */
export const startEnvelope = (): void => {
  const desk = document.querySelector<HTMLElement>("[data-envelope]");
  if (!desk) {
    return;
  }
  const status = document.querySelector<HTMLElement>("[data-envelope-status]");
  const choices = [
    ...desk.querySelectorAll<HTMLInputElement>("input[name='to']"),
  ];
  const parts = [...desk.querySelectorAll<HTMLElement>("[data-to]")];
  desk.classList.add("is-live");

  /**
   * Addresses the envelope to a team. A visible change (`stamp`) inks the postmark again and says
   * who the envelope is now for; the address the page opens with is simply there.
   */
  const address = (id: string, stamp: boolean): void => {
    for (const part of parts) {
      part.hidden = part.dataset.to !== id;
    }
    if (!stamp) {
      return;
    }
    replay(desk, "is-stamping");
    const team = choices
      .find((choice) => choice.value === id)
      ?.closest("label");
    if (status && team) {
      status.textContent = `Addressed to ${team.querySelector(".team-name")?.textContent ?? id}`;
    }
  };

  desk.addEventListener("change", (event) => {
    if (
      event.target instanceof HTMLInputElement &&
      event.target.name === "to"
    ) {
      address(event.target.value, true);
    }
  });

  /** Addresses the envelope to the team the page's link names, or else to the checked one. */
  const follow = (stamp: boolean): void => {
    const named = choices.find(
      (choice) => choice.value === window.location.hash.slice(1)
    );
    if (named) {
      named.checked = true;
    }
    // A browser that restores the form on the way back may have checked another team.
    const chosen = named ?? choices.find((choice) => choice.checked);
    if (chosen) {
      address(chosen.value, stamp);
    }
  };
  follow(false);
  window.addEventListener("hashchange", () => {
    follow(true);
  });
  window.addEventListener("pageshow", (event) => {
    // Back from the browser's page cache, the radios may no longer match the envelope.
    if (event.persisted) {
      follow(false);
    }
  });

  for (const button of desk.querySelectorAll<HTMLButtonElement>(
    "[data-copy-address]"
  )) {
    const label = button.querySelector("span");
    const text = label?.textContent ?? "";
    let timer = 0;
    const copy = async (): Promise<void> => {
      const copied = button.dataset.copyAddress ?? "";
      await navigator.clipboard.writeText(copied);
      button.dataset.copied = "";
      if (label) {
        label.textContent = "Copied";
      }
      if (status) {
        status.textContent = `${copied} copied`;
      }
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        delete button.dataset.copied;
        if (label) {
          label.textContent = text;
        }
      }, SAY_FOR);
    };
    button.hidden = false;
    button.addEventListener("click", () => {
      void copy();
    });
  }

  for (const link of desk.querySelectorAll<HTMLAnchorElement>(
    "a[href^='mailto:']"
  )) {
    link.addEventListener("click", () => {
      replay(desk, "is-sending");
    });
  }
};
