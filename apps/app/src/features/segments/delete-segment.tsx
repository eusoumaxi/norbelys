import type { SegmentObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { segmentKey, segmentsKey } from "@/features/segments/queries";
import { useAction } from "@/lib/actions";
import { useWorkspace } from "@/lib/workspace";

/**
 * Asks before `segments.delete`: the saved filter goes, its people and the enrollments already
 * made from it stay. `onDeleted` runs after the API agreed (a detail page leaves for the list).
 */
export const DeleteSegmentDialog = ({
  onDeleted,
  onOpenChange,
  open,
  segment,
}: {
  onDeleted?: () => Promise<unknown>;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  segment: SegmentObject | null;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  return (
    <ConfirmDialog
      confirmLabel="Delete segment"
      danger
      description={
        <>
          Deleting{" "}
          <span className="text-fg font-semibold">{segment?.name}</span> removes
          the saved filter. Its people stay, and enrollments already made from
          it are kept.
        </>
      }
      onConfirm={() =>
        segment
          ? action(
              "Segment deleted",
              () => workspace.api.segments.delete(segment.id),
              async () => {
                await onDeleted?.();
                queryClient.removeQueries({
                  queryKey: segmentKey(workspace, segment.id),
                });
                void queryClient.invalidateQueries({
                  queryKey: segmentsKey(workspace),
                });
              }
            )
          : undefined
      }
      onOpenChange={onOpenChange}
      open={open}
      title="Delete segment"
    />
  );
};
