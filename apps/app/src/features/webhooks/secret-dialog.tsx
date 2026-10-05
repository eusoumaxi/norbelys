import { useState } from "react";
import type { ReactNode } from "react";

import { CodeLine } from "@/components/copy";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

/**
 * The signing secret, as the one response that carries it gave it: shown once, in a box with
 * its copy button. The API never returns it again, so the person copies it before closing.
 */
export const SecretReveal = ({ secret }: { secret: string }) => (
  <div className="flex flex-col gap-2">
    <CodeLine prefix={null} value={secret} />
    <p className="text-fg-3 text-xs">
      Keep it in your server&apos;s environment (for example as
      NORBELYS_WEBHOOK_SECRET). It is shown once: it can be rotated, never read
      again.
    </p>
  </div>
);

/**
 * A dialog that shows a secret just made (a rotation's) until the person closes it. `secret`
 * opens it; closing clears it, which forgets the secret in this page as well (the box keeps
 * it while the dialog fades out).
 */
export const SecretDialog = ({
  children,
  onClose,
  secret,
  title,
}: {
  children?: ReactNode;
  onClose: () => void;
  secret: string | null;
  title: string;
}) => {
  const [shown, setShown] = useState(secret);
  if (secret !== null && secret !== shown) {
    setShown(secret);
  }
  return (
    <Dialog
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
      onOpenChangeComplete={(open) => {
        if (!open) {
          setShown(null);
        }
      }}
      open={secret !== null}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            Every delivery is signed with it; verify the signature before
            trusting a request.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          {shown ? <SecretReveal secret={shown} /> : null}
          {children}
        </DialogBody>
        <DialogFooter>
          <DialogClose render={<Button variant="primary" />}>
            I&apos;ve copied it
          </DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
