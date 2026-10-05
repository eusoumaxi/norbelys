import type { DomainObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { domainsKey } from "@/features/domains/queries";
import { useAction } from "@/lib/actions";
import { useWorkspace } from "@/lib/workspace";

/**
 * Asks before deleting a sending domain, saying what goes with it: its hostname is released, and
 * campaigns that served links from it use the default tracking host for their new messages.
 * `onRemoved` runs once the API deleted it.
 */
export const RemoveDomainDialog = ({
  domain,
  onOpenChange,
  onRemoved,
  open,
}: {
  domain: DomainObject | null;
  onOpenChange: (open: boolean) => void;
  onRemoved?: () => Promise<unknown> | undefined;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  return (
    <ConfirmDialog
      confirmLabel="Remove domain"
      danger
      description={
        <>
          {domain?.hostname} is deleted from this workspace and its hostname is
          released. New messages of campaigns that served their links from it
          use the default tracking host. Its DNS records can be taken down
          afterwards.
        </>
      }
      onConfirm={() =>
        domain
          ? action(
              "Sending domain removed",
              () => workspace.api.sendingDomains.delete(domain.id),
              async () => {
                await onRemoved?.();
                await queryClient.invalidateQueries({
                  queryKey: domainsKey(workspace),
                });
              }
            )
          : undefined
      }
      onOpenChange={onOpenChange}
      open={open}
      title={`Remove ${domain?.hostname ?? "this domain"}?`}
    />
  );
};
