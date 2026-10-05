import type { PersonObject } from "@norbelys/sdk";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { z } from "zod";

import { DialogActions } from "@/components/dialog-actions";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FieldGroup } from "@/components/ui/field";
import { peopleKey, personKey } from "@/features/people/queries";
import { toFormErrors, useAppForm } from "@/lib/form";
import { useWorkspace } from "@/lib/workspace";

const NAME_MAX = "Use 200 characters or fewer.";

const schema = z.object({
  company: z.string().max(200, NAME_MAX),
  email: z
    .string()
    .trim()
    .min(1, "Enter an email address.")
    .max(320, "Use 320 characters or fewer."),
  family_name: z.string().max(200, NAME_MAX),
  given_name: z.string().max(200, NAME_MAX),
});

/** An empty name clears it: `null` is how the API removes a value. */
const cleared = (value: string): string | null => value.trim() || null;

const PersonForm = ({
  onSaved,
  person,
}: {
  onSaved: () => void;
  person: PersonObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const form = useAppForm({
    defaultValues: {
      company: person.company ?? "",
      email: person.email,
      family_name: person.family_name ?? "",
      given_name: person.given_name ?? "",
    },
    onSubmit: () => {
      toast.success("Person saved");
      onSaved();
    },
    validators: {
      onChange: schema,
      onSubmitAsync: async ({ value }) => {
        try {
          const saved = await workspace.api.people.update(
            person.id,
            {
              company: cleared(value.company),
              email: value.email.trim(),
              family_name: cleared(value.family_name),
              given_name: cleared(value.given_name),
            },
            { headers: { "If-Match": `"${person.version}"` } }
          );
          queryClient.setQueryData(personKey(workspace, saved.id), saved);
          void queryClient.invalidateQueries({
            queryKey: [...peopleKey(workspace), "list"],
          });
          return null;
        } catch (error) {
          return toFormErrors(error);
        }
      },
    },
  });

  return (
    <form.AppForm>
      <form.DialogForm>
        <DialogBody>
          <FieldGroup>
            <form.AppField name="email">
              {(field) => (
                <field.TextField
                  autoComplete="off"
                  description="Unique in the workspace, ignoring case."
                  label="Email"
                  requirement="required"
                  type="email"
                />
              )}
            </form.AppField>
            <div className="grid gap-4 sm:grid-cols-2">
              <form.AppField name="given_name">
                {(field) => (
                  <field.TextField label="First name" requirement="optional" />
                )}
              </form.AppField>
              <form.AppField name="family_name">
                {(field) => (
                  <field.TextField label="Last name" requirement="optional" />
                )}
              </form.AppField>
            </div>
            <form.AppField name="company">
              {(field) => (
                <field.TextField label="Company" requirement="optional" />
              )}
            </form.AppField>
          </FieldGroup>
          <form.FormError />
        </DialogBody>
        <DialogActions>
          <form.SubmitButton>Save changes</form.SubmitButton>
        </DialogActions>
      </form.DialogForm>
    </form.AppForm>
  );
};

/**
 * Edits a person's address, names and company with `people.update`, sent with the person's
 * version in `If-Match` so a change made meanwhile is never overwritten: the API refuses the save
 * and says so. An emptied name is cleared. Custom values and groups are edited on the page.
 */
export const PersonDialog = ({
  onOpenChange,
  open,
  person,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
  person: PersonObject;
}) => (
  <Dialog onOpenChange={onOpenChange} open={open}>
    <DialogContent>
      <DialogHeader>
        <DialogTitle>Edit person</DialogTitle>
        <DialogDescription>
          Changing the address keeps the person&apos;s history and groups.
        </DialogDescription>
      </DialogHeader>
      <PersonForm
        key={person.version}
        onSaved={() => onOpenChange(false)}
        person={person}
      />
    </DialogContent>
  </Dialog>
);
