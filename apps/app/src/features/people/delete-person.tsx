import type { PersonObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { ConfirmDialog } from "@/components/confirm-dialog";
import { groupsKey } from "@/features/groups/queries";
import { peopleKey, personKey } from "@/features/people/queries";
import { useAction } from "@/lib/actions";
import { useWorkspace } from "@/lib/workspace";

/**
 * Asks before `people.delete`, which removes the person and their group memberships; the API
 * refuses one whose history (messages, enrollments) refers to them. `onDeleted` runs once the API
 * agreed and before the person's queries are dropped: a page that shows the person leaves first,
 * so nothing reads the deleted person again.
 */
export const DeletePersonDialog = ({
  onDeleted,
  onOpenChange,
  open,
  person,
}: {
  onDeleted?: () => Promise<unknown>;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  person: PersonObject | null;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const action = useAction();
  // The address stays in the dialog while it closes.
  const [shown, setShown] = useState(person);
  if (person && person !== shown) {
    setShown(person);
  }
  return (
    <ConfirmDialog
      confirmLabel="Delete person"
      danger
      description={
        <>
          Deleting <span className="text-fg font-semibold">{shown?.email}</span>{" "}
          removes them and their group memberships. A person with messages or
          enrollments can&apos;t be deleted: their history refers to them.
        </>
      }
      onConfirm={() =>
        person
          ? action(
              "Person deleted",
              () => workspace.api.people.delete(person.id),
              async () => {
                await onDeleted?.();
                queryClient.removeQueries({
                  queryKey: personKey(workspace, person.id),
                });
                void queryClient.invalidateQueries({
                  queryKey: [...peopleKey(workspace), "list"],
                });
                void queryClient.invalidateQueries({
                  queryKey: groupsKey(workspace),
                });
              }
            )
          : undefined
      }
      onOpenChange={onOpenChange}
      open={open}
      title="Delete person"
    />
  );
};
