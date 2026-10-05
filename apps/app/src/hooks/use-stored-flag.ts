import { useCallback, useEffect, useState } from "react";

const CHANGED = "nb.preference.changed";

/**
 * A yes or no this browser remembers across visits (in `localStorage`, as `"1"` or `"0"`), such as
 * a collapsed sidebar or a dismissed card. Where the browser refuses storage (a private window), it
 * starts with the supplied default and lasts for the page only. Mounted copies and other browser
 * tabs follow storage changes, so desktop and mobile navigation share the same preference.
 */
export const useStoredFlag = (
  key: string,
  initial = false
): [boolean, (value: boolean) => void] => {
  const [value, setValue] = useState(() => {
    try {
      const stored = localStorage.getItem(key);
      return stored === null ? initial : stored === "1";
    } catch {
      return initial;
    }
  });
  useEffect(() => {
    const read = () => {
      try {
        const stored = localStorage.getItem(key);
        setValue(stored === null ? initial : stored === "1");
      } catch {
        // Storage refused: keep the value already held by this page.
      }
    };
    window.addEventListener("storage", read);
    window.addEventListener(CHANGED, read);
    return () => {
      window.removeEventListener("storage", read);
      window.removeEventListener(CHANGED, read);
    };
  }, [initial, key]);
  const set = useCallback(
    (next: boolean) => {
      setValue(next);
      try {
        localStorage.setItem(key, next ? "1" : "0");
        window.dispatchEvent(new Event(CHANGED));
      } catch {
        // Storage refused: the value lasts for this page only.
      }
    },
    [key]
  );
  return [value, set];
};
