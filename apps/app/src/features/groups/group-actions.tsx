import { UserGroupIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { GroupObject } from "@norbelys/sdk";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useId, useState } from "react";
import { toast } from "sonner";

import { DialogActions } from "@/components/dialog-actions";
import { ProblemAlert } from "@/components/problem";
import { CopyIdItem, RowMenu } from "@/components/row-menu";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { groupsKey } from "@/features/groups/queries";
import { problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

interface DeleteGroupDialogProps {
  group: GroupObject;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}

/** Deleting asks for a tick first: the group can't come back. */
const DeleteGroupDialog = ({
  group,
  onOpenChange,
  open,
}: DeleteGroupDialogProps) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const confirmId = useId();
  const [confirmed, setConfirmed] = useState(false);
  const remove = useMutation({
    mutationFn: () => workspace.api.groups.delete(group.id),
    onSuccess: () => {
      onOpenChange(false);
      toast.success(`Deleted “${group.name}”`);
      void queryClient.invalidateQueries({ queryKey: groupsKey(workspace) });
    },
  });

  return (
    <Dialog
      onOpenChange={(next) => {
        if (!remove.isPending) {
          onOpenChange(next);
        }
      }}
      onOpenChangeComplete={(next) => {
        if (!next) {
          setConfirmed(false);
          remove.reset();
        }
      }}
      open={open}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete group</DialogTitle>
          <DialogDescription>
            The group and its member list are removed; the people in it stay in
            your workspace.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="border-line bg-chrome flex items-center gap-3 rounded-sm border p-3">
            <span className="bg-surface text-icon flex size-7 shrink-0 items-center justify-center rounded-sm">
              <HugeiconsIcon className="size-4" icon={UserGroupIcon} />
            </span>
            <span className="flex min-w-0 flex-col">
              <span className="text-fg truncate text-sm font-semibold">
                {group.name}
              </span>
              <span className="text-fg-3 truncate font-mono text-xs">
                {group.id}
              </span>
            </span>
          </div>
          {remove.isError ? (
            <ProblemAlert>{problemLine(remove.error)}</ProblemAlert>
          ) : null}
          <label
            className="text-fg-2 flex cursor-pointer items-center gap-2 text-sm"
            htmlFor={confirmId}
          >
            <Checkbox
              checked={confirmed}
              id={confirmId}
              onCheckedChange={(value) => setConfirmed(value)}
              variant="destructive"
            />
            I understand this cannot be undone
          </label>
        </DialogBody>
        <DialogActions>
          <Button
            disabled={!confirmed || remove.isPending}
            onClick={() => remove.mutate()}
            variant="danger"
          >
            {remove.isPending ? <Spinner /> : null}
            Delete group
          </Button>
        </DialogActions>
      </DialogContent>
    </Dialog>
  );
};

interface GroupActionsProps {
  group: GroupObject;
  onEdit: () => void;
}

/** The row's "⋯" menu. */
export const GroupActions = ({ group, onEdit }: GroupActionsProps) => {
  const [deleting, setDeleting] = useState(false);
  const workspace = useWorkspace();
  const navigate = useNavigate();
  return (
    <>
      <RowMenu label={`Actions for ${group.name}`}>
        <DropdownMenuItem
          onClick={() => {
            void navigate({
              params: { slug: workspace.slug },
              search: { group: group.id },
              to: "/w/$slug/people",
            });
          }}
        >
          View people
        </DropdownMenuItem>
        <DropdownMenuItem onClick={onEdit}>Edit group</DropdownMenuItem>
        <CopyIdItem id={group.id} noun="group" />
        <DropdownMenuItem
          className="text-error-fg"
          onClick={() => setDeleting(true)}
        >
          Delete group
        </DropdownMenuItem>
      </RowMenu>
      <DeleteGroupDialog
        group={group}
        onOpenChange={setDeleting}
        open={deleting}
      />
    </>
  );
};
