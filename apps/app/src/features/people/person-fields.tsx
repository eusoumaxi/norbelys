import type { FieldObject, PersonObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";

import { Dash } from "@/components/data-table";
import { Problem, ProblemAlert } from "@/components/problem";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Field, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { describeValue, draftOf, valueOf } from "@/features/people/fields";
import { fieldsQuery, peopleKey, personKey } from "@/features/people/queries";
import { plural } from "@/lib/format";
import { fieldProblems, problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

interface ControlProps {
  definition: FieldObject;
  id: string;
  invalid: boolean;
  onChange: (value: string) => void;
  value: string;
}

/** A yes/no field: a switch, its state in words, and a way back to "not set". */
const BooleanControl = ({ id, onChange, value }: ControlProps) => {
  let state = "Not set";
  if (value === "true") {
    state = "Yes";
  } else if (value === "false") {
    state = "No";
  }
  return (
    <div className="flex h-8 items-center gap-3">
      <Switch
        checked={value === "true"}
        id={id}
        onCheckedChange={(checked) => onChange(checked ? "true" : "false")}
      />
      <span className={value ? "text-fg text-sm" : "text-fg-3 text-sm"}>
        {state}
      </span>
      {value ? (
        <Button onClick={() => onChange("")} size="s" variant="tertiary">
          Clear
        </Button>
      ) : null}
    </div>
  );
};

/** One custom field's control, chosen by the field's type. */
const FieldControl = (props: ControlProps) => {
  const { definition, id, invalid, onChange, value } = props;
  if (definition.type === "boolean") {
    return <BooleanControl {...props} />;
  }
  if (definition.type === "enum") {
    return (
      <Select
        id={id}
        onChange={onChange}
        options={[
          { label: "Not set", value: "" },
          ...definition.options.map((option) => ({
            label: option,
            value: option,
          })),
        ]}
        value={value}
      />
    );
  }
  let type = "text";
  if (definition.type === "number") {
    type = "number";
  } else if (definition.type === "date") {
    type = "date";
  }
  return (
    <Input
      aria-invalid={invalid}
      id={id}
      maxLength={definition.type === "text" ? 1000 : undefined}
      onChange={(event) => onChange(event.target.value)}
      step={definition.type === "number" ? "any" : undefined}
      type={type}
      value={value}
    />
  );
};

/** The label of a field's control, with its key in mono for people who write to the API. */
const FieldName = ({
  definition,
  id,
}: {
  definition: FieldObject;
  id: string;
}) => (
  <FieldLabel className="justify-between gap-2" htmlFor={id}>
    <span className="truncate">{definition.label}</span>
    <code className="text-fg-3 truncate font-mono text-xs font-normal">
      {definition.key}
    </code>
  </FieldLabel>
);

/**
 * The editable form. It keeps only the values edited and not saved yet, over the person as the
 * cache holds them, so a change elsewhere on the page (their groups) never discards an edit.
 */
const FieldsForm = ({
  definitions,
  person,
}: {
  definitions: FieldObject[];
  person: PersonObject;
}) => {
  const workspace = useWorkspace();
  const queryClient = useQueryClient();
  const [edits, setEdits] = useState<Record<string, string>>({});
  const valueFor = (definition: FieldObject): string =>
    edits[definition.key] ?? draftOf(person.fields[definition.key]);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const changed = definitions.filter(
    (definition) =>
      Object.hasOwn(edits, definition.key) &&
      valueFor(definition) !== draftOf(person.fields[definition.key])
  );
  const problems = fieldProblems(failure);
  const unplaced =
    failure && !definitions.some((d) => problems[`fields.${d.key}`]);

  const save = async () => {
    setSaving(true);
    setFailure(null);
    const fields = Object.fromEntries(
      changed.map((definition) => [
        definition.key,
        valueOf(definition.type, valueFor(definition)),
      ])
    );
    try {
      const saved = await workspace.api.people.update(
        person.id,
        { fields },
        { headers: { "If-Match": `"${person.version}"` } }
      );
      toast.success("Profile saved");
      setEdits({});
      queryClient.setQueryData(personKey(workspace, saved.id), saved);
      void queryClient.invalidateQueries({
        queryKey: [...peopleKey(workspace), "list"],
      });
    } catch (error) {
      setFailure(error);
    }
    setSaving(false);
  };

  return (
    <>
      <CardContent className="flex flex-col gap-4">
        <div className="grid gap-x-6 gap-y-4 md:grid-cols-2">
          {definitions.map((definition) => {
            const id = `field-${definition.key}`;
            const problem = problems[`fields.${definition.key}`];
            return (
              <Field key={definition.id}>
                <FieldName definition={definition} id={id} />
                <FieldControl
                  definition={definition}
                  id={id}
                  invalid={Boolean(problem)}
                  onChange={(value) =>
                    setEdits((current) => ({
                      ...current,
                      [definition.key]: value,
                    }))
                  }
                  value={valueFor(definition)}
                />
                {problem ? <FieldError>{problem}</FieldError> : null}
              </Field>
            );
          })}
        </div>
        {unplaced ? <ProblemAlert>{problemLine(failure)}</ProblemAlert> : null}
      </CardContent>
      {changed.length > 0 ? (
        <CardFooter className="justify-end gap-2">
          <span className="text-fg-3 mr-auto text-xs">
            {plural(changed.length, "unsaved change")}
          </span>
          <Button
            disabled={saving}
            onClick={() => {
              setEdits({});
              setFailure(null);
            }}
            size="s"
            variant="tertiary"
          >
            Discard
          </Button>
          <Button
            disabled={saving}
            onClick={() => {
              void save();
            }}
            size="s"
            variant="primary"
          >
            {saving ? <Spinner /> : null}
            Save changes
          </Button>
        </CardFooter>
      ) : null}
    </>
  );
};

/** The values as text, for people who may read the audience but not change it. */
const FieldsList = ({
  definitions,
  person,
}: {
  definitions: FieldObject[];
  person: PersonObject;
}) => (
  <CardContent>
    <dl className="grid gap-x-6 gap-y-3 md:grid-cols-2">
      {definitions.map((definition) => {
        const value = person.fields[definition.key];
        return (
          <div className="flex min-w-0 flex-col gap-0.5" key={definition.id}>
            <dt className="text-fg-2 text-sm font-semibold">
              {definition.label}
            </dt>
            <dd className="text-fg text-sm break-words">
              {value === null || value === undefined ? (
                <Dash />
              ) : (
                describeValue(value)
              )}
            </dd>
          </div>
        );
      })}
    </dl>
  </CardContent>
);

/** What stands in the card while the definitions load, fail, or do not exist yet. */
const FieldsBody = ({
  editable,
  person,
}: {
  editable: boolean;
  person: PersonObject;
}) => {
  const workspace = useWorkspace();
  const fields = useQuery(fieldsQuery(workspace));
  if (fields.isPending) {
    return (
      <CardContent className="grid gap-4 md:grid-cols-2">
        <Skeleton className="h-14" />
        <Skeleton className="h-14" />
      </CardContent>
    );
  }
  if (fields.isError) {
    return (
      <Problem
        error={fields.error}
        onRetry={() => {
          void fields.refetch();
        }}
      />
    );
  }
  if (fields.data.length === 0) {
    return (
      <CardContent>
        <p className="text-fg-3 text-sm">
          No custom fields yet.{" "}
          <Link
            className="text-link hover:text-link-hover font-semibold"
            params={{ slug: workspace.slug }}
            to="/w/$slug/fields"
          >
            Define fields
          </Link>{" "}
          to keep typed attributes such as an industry, a plan or a renewal
          date.
        </p>
      </CardContent>
    );
  }
  return editable ? (
    <FieldsForm definitions={fields.data} person={person} />
  ) : (
    <FieldsList definitions={fields.data} person={person} />
  );
};

/**
 * A person's custom values, each with the control its definition's type calls for (text, a
 * number, a switch for yes/no, a select of an enum's options, a date). Only what changed is sent,
 * as `people.update({fields})`, which merges it; an emptied control sends `null`, which removes
 * the value. The person's version goes in `If-Match`, so a save never undoes a change made
 * meanwhile, and the API's message for a refused value shows under its control.
 */
export const PersonFields = ({
  editable,
  person,
}: {
  editable: boolean;
  person: PersonObject;
}) => (
  <Card>
    <CardHeader className="flex-col items-start gap-0.5">
      <CardTitle>Profile</CardTitle>
      <CardDescription className="text-xs">
        Custom fields, typed by their definitions. Campaigns can use them as
        variables and segments can filter on them.
      </CardDescription>
    </CardHeader>
    <FieldsBody editable={editable} person={person} />
  </Card>
);
