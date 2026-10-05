import type { FieldObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { peopleKey, fieldsKey } from "@/features/people/queries";
import { segmentOptionsQuery } from "@/features/segments/queries";
import { useAction } from "@/lib/actions";
import { useWorkspace } from "@/lib/workspace";

/**
 * The segments whose filter names the field, from the first 100: the API refuses to delete a
 * field a segment uses, so the dialog says which ones before the person tries.
 */
const useSegmentsUsing = (field: FieldObject | null, open: boolean) => {
  const workspace = useWorkspace();
  const segments = useQuery({
    ...segmentOptionsQuery(workspace),
    enabled: open,
  });
  if (!field) {
    return [];
  }
  return (segments.data ?? []).filter((segment) =>
    segment.filter.conditions.some(
      (condition) => condition.field === `fields.${field.key}`
    )
  );
};

/**
 * Asks before `fields.delete`, which removes the definition and every person's value of it in one
 * go, for good. The key must be typed to confirm. A field a segment's filter uses cannot be
 * deleted; the segments that do are named, with links, so their filters can be changed first.
 */
export const DeleteFieldDialog = ({
  field,
  onOpenChange,
  open,
}: {
  field: FieldObject | null;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  const users = useSegmentsUsing(field, open);
  return (
    <ConfirmDialog
      confirmLabel="Delete field"
      confirmText={field?.key}
      danger
      description={
        <>
          Deleting <span className="text-fg font-semibold">{field?.label}</span>{" "}
          removes the field and every person&apos;s value of it, at once.
          Imports stop reading its column and campaigns can no longer use it as
          a variable. This can&apos;t be undone.
        </>
      }
      onConfirm={() =>
        field
          ? action(
              `Deleted “${field.label}”`,
              () => workspace.api.fields.delete(field.id),
              () => {
                void queryClient.invalidateQueries({
                  queryKey: fieldsKey(workspace),
                });
                void queryClient.invalidateQueries({
                  queryKey: peopleKey(workspace),
                });
              }
            )
          : undefined
      }
      onOpenChange={onOpenChange}
      open={open}
      title="Delete field"
    >
      {users.length > 0 ? (
        <Alert variant="warning">
          <AlertTitle>
            Used by{" "}
            {users.length === 1 ? "a segment" : `${users.length} segments`}
          </AlertTitle>
          <AlertDescription>
            A field a segment filters on can&apos;t be deleted. Change the
            filter of{" "}
            {users.map((segment, index) => (
              <span key={segment.id}>
                {index > 0 ? ", " : null}
                <Link
                  params={{ segmentId: segment.id, slug: workspace.slug }}
                  to="/w/$slug/segments/$segmentId"
                >
                  {segment.name}
                </Link>
              </span>
            ))}{" "}
            first.
          </AlertDescription>
        </Alert>
      ) : null}
    </ConfirmDialog>
  );
};
