import { useState } from "react";
import type { ReactNode } from "react";

import { DialogActions } from "@/components/dialog-actions";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";

/**
 * Asks before a change that is hard to take back. With `confirmText`, the person types it (a name,
 * a slug) before the button wakes; with `blocked`, the button stays off (the dialog's own text
 * says what is missing). `onConfirm` runs the change; the dialog closes when it settles.
 */
export const ConfirmDialog = ({
  blocked = false,
  children,
  confirmLabel,
  confirmText,
  danger = false,
  description,
  onConfirm,
  onOpenChange,
  open,
  title,
}: {
  blocked?: boolean;
  children?: ReactNode;
  confirmLabel: string;
  confirmText?: string;
  danger?: boolean;
  description: ReactNode;
  onConfirm: () => unknown;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  title: string;
}) => {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const ready = !confirmText || typed === confirmText;
  return (
    <Dialog
      onOpenChange={(next) => {
        if (!busy) {
          onOpenChange(next);
          setTyped("");
        }
      }}
      open={open}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        {children || confirmText ? (
          <DialogBody>
            {children}
            {confirmText ? (
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="confirm-text">
                  Type{" "}
                  <code className="text-fg mx-1 font-mono">{confirmText}</code>{" "}
                  to confirm
                </Label>
                <Input
                  autoComplete="off"
                  className="font-mono"
                  id="confirm-text"
                  onChange={(event) => setTyped(event.target.value)}
                  value={typed}
                />
              </div>
            ) : null}
          </DialogBody>
        ) : null}
        <DialogActions>
          <Button
            disabled={!ready || busy || blocked}
            onClick={async () => {
              setBusy(true);
              try {
                await onConfirm();
              } catch {
                // The change reports its own failure; the dialog stays open to try again.
                setBusy(false);
                return;
              }
              setBusy(false);
              setTyped("");
              onOpenChange(false);
            }}
            variant={danger ? "danger" : "primary"}
          >
            {busy ? <Spinner /> : null}
            {confirmLabel}
          </Button>
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
};
