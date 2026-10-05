/**
 * The SDR page's to-do list: each task ticks itself off as it scrolls into the middle of the
 * screen, and the count beside the list goes down with it, to the one task that's left. Without
 * motion the list is already done and the count already says one.
 */

/** Starts the list, if the page has one and motion is welcome. */
export const startTodo = (): void => {
  const list = document.querySelector<HTMLElement>("[data-todo]");
  const count = list?.querySelector<HTMLElement>("[data-todo-count]");
  if (
    !(list && count) ||
    !document.documentElement.classList.contains("motion")
  ) {
    return;
  }
  const items = [...list.querySelectorAll<HTMLElement>("[data-todo-item]")];
  const tasks = items.filter((item) => !item.classList.contains("todo-yours"));
  const yours = items.length - tasks.length;
  const show = (): void => {
    const left = tasks.filter(
      (task) => !task.classList.contains("is-done")
    ).length;
    count.textContent = String(left + yours);
  };
  show();
  const watcher = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          entry.target.classList.add("is-done");
          watcher.unobserve(entry.target);
        }
      }
      show();
    },
    { rootMargin: "0px 0px -38% 0px", threshold: 1 }
  );
  for (const item of items) {
    watcher.observe(item);
  }
};
