import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import { describeProblem } from "@/lib/problem";

/**
 * Runs a change started from a button, a menu or a dialog: a toast says how it went (the API's own
 * words on failure), then the queries under `refresh` load again (or `refresh` runs, when it is a
 * function). The promise settles once all of it is done, with whether the change was made; it
 * never rejects, so a click may leave it running and a dialog may wait for it.
 */
export const useAction = () => {
  const queryClient = useQueryClient();
  return async (
    label: string,
    task: () => Promise<unknown>,
    refresh?: readonly unknown[] | (() => unknown)
  ): Promise<boolean> => {
    try {
      await task();
      toast.success(label);
      if (typeof refresh === "function") {
        await refresh();
      } else if (refresh) {
        await queryClient.invalidateQueries({ queryKey: refresh });
      }
      return true;
    } catch (error) {
      toast.error(describeProblem(error).detail);
      return false;
    }
  };
};

/** Copies text and says so. */
export const copyText = (
  text: string,
  label = "Copied to the clipboard"
): void => {
  const run = async () => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(label);
    } catch {
      toast.error("Couldn’t copy to the clipboard");
    }
  };
  void run();
};
