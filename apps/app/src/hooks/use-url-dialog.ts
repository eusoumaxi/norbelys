import { parseAsString, useQueryState } from "nuqs";
import { useState } from "react";

/**
 * A dialog opened by `?key=value`: it survives a reload, can be linked, and the browser's
 * back button closes it. `value` keeps the last target while the closing animation plays.
 */
export const useUrlDialog = (key: string) => {
  const [param, setParam] = useQueryState(
    key,
    parseAsString.withOptions({ history: "push" })
  );
  const [value, setValue] = useState(param);
  if (param !== null && param !== value) {
    setValue(param);
  }

  const close = () => {
    void setParam(null);
  };

  return {
    close,
    open: (target: string) => {
      void setParam(target);
    },
    /** Spread on a shadcn `Dialog`. */
    props: {
      onOpenChange: (open: boolean) => {
        if (!open) {
          close();
        }
      },
      onOpenChangeComplete: (open: boolean) => {
        if (!open) {
          setValue(null);
        }
      },
      open: param !== null,
    },
    value,
  };
};
