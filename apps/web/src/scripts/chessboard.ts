/**
 * The account executive's board (components/teams/ae-board.astro). The stylesheet plays the
 * opening on its own: the board draws itself, the pieces rise, the moves travel and the Head of
 * RevOps answers. This script hands the board to the visitor: pick any piece and it becomes the
 * one who replied, with the pink ring and the speech bubble, while everyone else at the company
 * is paused; "Reset the board" takes every answer back and sends the moves out again. The status
 * line says what happened, for everyone and for screen readers.
 *
 * Under reduced motion the same choices apply at once, without the moves travelling.
 */

type PieceState = "waiting" | "replied" | "paused";

/** Where a piece stands once `replied` has answered, or before anyone has. */
const stateOf = (
  id: string | undefined,
  replied: string | undefined
): PieceState => {
  if (replied === undefined) {
    return "waiting";
  }
  return id === replied ? "replied" : "paused";
};

/** Starts the board, if the page has one. */
export const startChessboard = (): void => {
  const hero = document.querySelector<HTMLElement>("[data-board]");
  const stage = hero?.querySelector<HTMLElement>("[data-board-stage]");
  const status = stage?.querySelector<HTMLElement>("[data-board-status]");
  const reset = stage?.querySelector<HTMLButtonElement>("[data-board-reset]");
  if (!(stage && status && reset)) {
    return;
  }
  const picks = [
    ...stage.querySelectorAll<HTMLButtonElement>("button[data-piece]"),
  ];
  // Everything drawn for a piece: its ring, its move, the piece, its button and its bubble.
  const parts = [
    ...stage.querySelectorAll<HTMLElement | SVGElement>("[data-piece]"),
  ];
  const motion = document.documentElement.classList.contains("motion");

  const show = (replied?: string): void => {
    for (const part of parts) {
      part.dataset.state = stateOf(part.dataset.piece, replied);
    }
    for (const pick of picks) {
      const chosen = pick.dataset.piece === replied;
      pick.setAttribute("aria-pressed", String(chosen));
      const tag = pick.querySelector<HTMLElement>("[data-board-tag-state]");
      if (tag) {
        tag.textContent = chosen ? "Replied" : "Paused";
      }
    }
    const who = picks.find((pick) => pick.dataset.piece === replied)?.dataset
      .title;
    const others = picks.length - 1;
    status.textContent = who
      ? `${who} replied. ${others} ${others === 1 ? "colleague" : "colleagues"} paused.`
      : `${picks.length} emails out. Nobody has replied yet.`;
  };

  /** From the first touch, the visitor plays: the opening's own timing gives way to theirs. */
  const takeOver = (): void => {
    stage.classList.add("is-live");
  };

  /** Sends the moves out again, one after another, from the pawn. */
  const replay = (): void => {
    if (!motion) {
      return;
    }
    stage.classList.remove("is-replaying");
    // Reading the layout restarts the moves' animation from its first frame.
    void stage.offsetWidth;
    stage.classList.add("is-replaying");
  };

  for (const pick of picks) {
    pick.addEventListener("click", () => {
      takeOver();
      stage.classList.remove("is-replaying");
      show(pick.dataset.piece);
    });
  }
  reset.addEventListener("click", () => {
    takeOver();
    show();
    replay();
  });
};
