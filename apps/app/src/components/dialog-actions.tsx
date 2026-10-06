import type { ReactNode } from "react";

import { Button } from "@/components/ui/button";
import { DialogClose, DialogFooter } from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";

/** A form's submit button: primary, off while `disabled`, and a spinner while it saves (`busy`). */
export const SubmitButton = ({
  busy,
  children,
  disabled = false,
}: {
  busy: boolean;
  children: ReactNode;
  disabled?: boolean;
}) => (
  <Button disabled={busy || disabled} type="submit" variant="primary">
    {busy ? <Spinner /> : null}
    {children}
  </Button>
);

/**
 * A dialog's footer: a short note on the left when there is one, Close, then the dialog's own
 * action when it has one (a `SubmitButton`, or a button that acts).
 */
export const DialogActions = ({
  children,
  disabled = false,
  note,
}: {
  children?: ReactNode;
  disabled?: boolean;
  note?: ReactNode;
}) => (
  <DialogFooter>
    {note ? <p className="text-fg-3 mr-auto text-xs">{note}</p> : null}
    <DialogClose disabled={disabled} render={<Button variant="secondary" />}>
      Close
    </DialogClose>
    {children}
  </DialogFooter>
);
