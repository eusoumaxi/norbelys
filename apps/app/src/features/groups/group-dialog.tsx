import type { GroupObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { z } from "zod";

import { DialogActions } from "@/components/dialog-actions";
import { DialogPending } from "@/components/problem";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FieldGroup } from "@/components/ui/field";
import { groupKey, groupQuery, groupsKey } from "@/features/groups/queries";
import { useUrlDialog } from "@/hooks/use-url-dialog";
import { toFormErrors, useAppForm } from "@/lib/form";
import { useWorkspace } from "@/lib/workspace";

/** `?group=new` opens the dialog empty; `?group=<id>` opens it on that group. */
export const NEW_GROUP = "new";

const schema = z.object({
  description: z.string().max(2000, "Use 2,000 characters or fewer."),
  name: z
    .string()
    .trim()
    .min(1, "Give the group a name.")
    .max(200, "Use 200 characters or fewer."),
});

interface GroupFormProps {
  group?: GroupObject;
  onSaved: () => void;
}

const GroupForm = ({ group, onSaved }: GroupFormProps) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const form = useAppForm({
    defaultValues: {
      description: group?.description ?? "",
      name: group?.name ?? "",
    },
    validators: {
      onChange: schema,
      onSubmitAsync: async ({ value }) => {
        const body = {
          description: value.description.trim() || null,
          name: value.name.trim(),
        };
        try {
          const saved = group
            ? await workspace.api.groups.update(group.id, body, {
                headers: { "If-Match": `"${group.version}"` },
              })
            : await workspace.api.groups.create(body);
          queryClient.setQueryData(groupKey(workspace, saved.id), saved);
          void queryClient.invalidateQueries({
            queryKey: groupsKey(workspace),
          });
          return null;
        } catch (error) {
          return toFormErrors(error);
        }
      },
    },
    onSubmit: () => {
      toast.success(group ? "Group saved" : "Group created");
      onSaved();
    },
  });

  return (
    <form.AppForm>
      <form.DialogForm>
        <DialogBody>
          <FieldGroup>
            <form.AppField name="name">
              {(field) => (
                <field.TextField label="Name" requirement="required" />
              )}
            </form.AppField>
            <form.AppField name="description">
              {(field) => (
                <field.TextareaField
                  description="Who belongs here, and why."
                  label="Description"
                  requirement="optional"
                />
              )}
            </form.AppField>
          </FieldGroup>
          <form.FormError />
        </DialogBody>
        <DialogActions note={group ? undefined : "The group starts empty."}>
          <form.SubmitButton>
            {group ? "Save changes" : "Create group"}
          </form.SubmitButton>
        </DialogActions>
      </form.DialogForm>
    </form.AppForm>
  );
};

const ExistingGroupForm = ({
  id,
  onSaved,
}: {
  id: string;
  onSaved: () => void;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  // Opened from the list, the row is already here; a deep link loads it (a small spinner).
  const group = useQuery(groupQuery(queryClient, workspace, id));
  if (!group.isSuccess) {
    return <DialogPending query={group} />;
  }
  return <GroupForm group={group.data} onSaved={onSaved} />;
};

/** Create or edit a group. Its open state is the `group` search parameter. */
export const GroupDialog = () => {
  const dialog = useUrlDialog("group");
  const isNew = dialog.value === NEW_GROUP;
  return (
    <Dialog {...dialog.props}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{isNew ? "Create group" : "Edit group"}</DialogTitle>
          <DialogDescription>
            {isNew
              ? "A list of people you choose, to use as a campaign audience."
              : "Renaming a group doesn't change who is in it."}
          </DialogDescription>
        </DialogHeader>
        {isNew ? <GroupForm onSaved={() => dialog.close()} /> : null}
        {dialog.value && !isNew ? (
          <ExistingGroupForm
            id={dialog.value}
            key={dialog.value}
            onSaved={() => dialog.close()}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
};
