import type { DomainObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { domainsKey } from "@/features/domains/queries";
import { useAction } from "@/lib/actions";
import { useWorkspace } from "@/lib/workspace";

/**
 * Explain domain removal before revoking managed mail or releasing a tracking hostname.
 * Existing mailbox history and separately configured tracking domains remain available.
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
          This removes {domain?.hostname} from this workspace.{" "}
          {domain?.purpose === "tracking"
            ? "New campaign messages will use the default tracking host."
            : "Managed sending and receiving for this domain will stop. Existing mailbox messages and history will be kept."}{" "}
          Remove only DNS records added for Norbelys; keep records used by your
          other services.
        </>
      }
      onConfirm={() =>
        domain
          ? action(
              "Domain removed",
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
