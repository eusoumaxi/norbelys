import {
  Alert02Icon,
  ArrowRight02Icon,
  FileImportIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { FieldObject } from "@norbelys/sdk";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { useState } from "react";
import type { ChangeEvent } from "react";

import { Illustration } from "@/components/illustration";
import { ProblemPanel } from "@/components/problem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { CsvFile, Target, UploadColumn } from "@/features/imports/csv";
import {
  ATTRIBUTES,
  byteLength,
  examples,
  guessTargets,
  IMPORT_BYTES_MAX,
  NEW_FIELD,
  newFieldKey,
  newFieldLabel,
  readCsv,
  repeatedTargets,
  SKIP,
  uploadCsv,
} from "@/features/imports/csv";
import { importsKey } from "@/features/imports/queries";
import {
  fieldsKey,
  fieldsQuery,
  groupOptionsQuery,
} from "@/features/people/queries";
import { FormField } from "@/lib/form";
import { plural } from "@/lib/format";
import { problemLine } from "@/lib/problem";
import { useWorkspace } from "@/lib/workspace";

/** The most custom fields a workspace holds. */
const FIELDS_MAX = 100;

/** One column of the chosen file, as the matching shows it. */
interface Column {
  index: number;
  /** The header, or `Column 3` when the header cell is empty. */
  name: string;
  /** Its first filled values. */
  examples: string[];
}

/** The file's columns worth showing: every column with a header or at least one value. */
const columnsOf = (file: CsvFile): Column[] =>
  file.header.flatMap((header, index) => {
    const values = examples(file.rows, index);
    if (!header && values.length === 0) {
      return [];
    }
    return [{ examples: values, index, name: header || `Column ${index + 1}` }];
  });

/** The first step: choose a CSV file, with the file picker or by dropping it on the zone. */
const ChooseFile = ({ onRead }: { onRead: (file: CsvFile) => void }) => {
  const [reading, setReading] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const take = async (chosen: File | undefined) => {
    if (!chosen) {
      return;
    }
    setReading(true);
    setProblem(null);
    try {
      const read = await readCsv(chosen);
      if (typeof read === "string") {
        setProblem(read);
      } else {
        onRead(read);
      }
    } catch {
      setProblem("The file could not be read. Choose it again.");
    }
    setReading(false);
  };
  return (
    <div className="flex flex-col gap-4">
      {/* The file input covers the whole zone: a click opens the picker, and a file dropped on
          it is taken natively. */}
      <label
        className={cn(
          "border-line-strong text-fg-2 hover:border-focus hover:bg-hover has-[input:focus-visible]:border-focus relative flex min-h-72 cursor-pointer flex-col items-center justify-center gap-3 rounded-sm border border-dashed px-6 py-10 text-center transition-colors",
          dragging ? "border-accent bg-hover" : null
        )}
        htmlFor="import-file"
      >
        {reading ? (
          <Spinner className="text-fg-3 my-10 size-6" />
        ) : (
          <Illustration className="w-[168px]" name="people" />
        )}
        <span className="text-fg text-base font-medium">
          Choose a CSV file or drop it here
        </span>
        <span className="max-w-md text-xs">
          One person per row, with a column of email addresses. The first row
          names the columns; names, company and anything else are optional.
        </span>
        <span
          aria-hidden
          className={cn(buttonVariants({ variant: "secondary" }), "mt-1")}
        >
          <HugeiconsIcon icon={FileImportIcon} />
          Choose file
        </span>
        <input
          accept=".csv,.tsv,.txt,text/csv,text/plain,text/tab-separated-values"
          aria-label="CSV file"
          className="absolute inset-0 size-full cursor-pointer opacity-0"
          id="import-file"
          onChange={(event: ChangeEvent<HTMLInputElement>) => {
            setDragging(false);
            void take(event.target.files?.[0]);
            event.target.value = "";
          }}
          onDragEnter={() => setDragging(true)}
          onDragLeave={() => setDragging(false)}
          onDrop={() => setDragging(false)}
          type="file"
        />
      </label>
      {problem ? (
        <Alert variant="error">
          <HugeiconsIcon icon={Alert02Icon} />
          <AlertTitle>Can&apos;t import this file</AlertTitle>
          <AlertDescription>{problem}</AlertDescription>
        </Alert>
      ) : null}
    </div>
  );
};

/** The fields a column can go into, labelled by name; the key is added only to tell two apart. */
const fieldOptions = (fields: readonly FieldObject[]) =>
  fields.map((field) => ({
    label:
      fields.filter((other) => other.label === field.label).length > 1 ||
      ATTRIBUTES.some((attribute) => attribute.label === field.label)
        ? `${field.label} (${field.key})`
        : field.label,
    value: `fields.${field.key}`,
  }));

/** What a target reads as: a person's detail, a field's label, or the column's own new field. */
const targetLabel = (target: Target, fields: readonly FieldObject[]): string =>
  ATTRIBUTES.find((attribute) => attribute.value === target)?.label ??
  fields.find((field) => `fields.${field.key}` === target)?.label ??
  target;

/** A column's first values in a line, or that it has none. */
const Examples = ({ column }: { column: Column }) =>
  column.examples.length > 0 ? (
    column.examples.join(", ")
  ) : (
    <span className="text-fg-4">Empty</span>
  );

/**
 * Each column of the file, its first values, and where it goes; the person changes what is wrong.
 * On a narrow screen the values move under the column's name, so the choice stays in view.
 */
const ColumnsTable = ({
  columns,
  fields,
  onChange,
  repeated,
  targets,
}: {
  columns: readonly Column[];
  fields: readonly FieldObject[];
  onChange: (index: number, target: Target) => void;
  repeated: readonly Target[];
  targets: readonly Target[];
}) => {
  const shared = [...ATTRIBUTES, ...fieldOptions(fields)];
  return (
    <Table className="table-fixed md:table-auto" shell>
      <TableHeader>
        <TableRow>
          <TableHead>In your file</TableHead>
          <TableHead className="hidden md:table-cell">First values</TableHead>
          <TableHead className="hidden w-6 px-0 md:table-cell">
            <span className="sr-only">Goes into</span>
          </TableHead>
          <TableHead className="w-1/2 md:w-[264px]">In Norbelys</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {columns.map((column) => {
          const target = targets[column.index] ?? SKIP;
          const skipped = target === SKIP;
          return (
            <TableRow key={column.index}>
              <TableCell className="max-w-[220px]">
                <span
                  className={cn(
                    "block truncate font-medium",
                    skipped ? "text-fg-3" : "text-fg"
                  )}
                >
                  {column.name}
                </span>
                <span className="text-fg-3 block truncate text-xs md:hidden">
                  <Examples column={column} />
                </span>
              </TableCell>
              <TableCell className="text-fg-3 hidden max-w-[300px] truncate md:table-cell">
                <Examples column={column} />
              </TableCell>
              <TableCell className="text-fg-4 hidden w-6 px-0 md:table-cell">
                <HugeiconsIcon className="size-4" icon={ArrowRight02Icon} />
              </TableCell>
              <TableCell>
                <Select
                  className={
                    repeated.includes(target) ? "border-error-line" : undefined
                  }
                  label={`Import “${column.name}” as`}
                  onChange={(value) => onChange(column.index, value)}
                  options={[
                    { label: "Don't import", value: SKIP },
                    {
                      label: `New field “${newFieldLabel(column.name, column.index)}”`,
                      value: NEW_FIELD,
                    },
                    ...shared,
                  ]}
                  value={target}
                />
              </TableCell>
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
};

/** Why the matching can't be imported yet, if it can't. */
const blockingProblem = (
  targets: readonly Target[],
  fields: readonly FieldObject[]
): string | null => {
  if (!targets.includes("email")) {
    return "Choose the column that holds the email addresses: every person needs one.";
  }
  const [first] = repeatedTargets(targets);
  if (first) {
    return `Two columns go into ${targetLabel(first, fields)}. Keep one, and set the other to Don't import.`;
  }
  const created = targets.filter((target) => target === NEW_FIELD).length;
  if (fields.length + created > FIELDS_MAX) {
    return `A workspace holds at most ${FIELDS_MAX} fields, and ${plural(created, "new field")} would pass that. Set some columns to Don't import.`;
  }
  return null;
};

/** One line on what the import will bring in. */
const summary = (columns: readonly Column[], targets: readonly Target[]) => {
  const imported = columns.filter(
    (column) => (targets[column.index] ?? SKIP) !== SKIP
  ).length;
  const created = columns.filter(
    (column) => targets[column.index] === NEW_FIELD
  ).length;
  const parts = [
    `${imported} of ${plural(columns.length, "column")} will be imported`,
  ];
  if (created > 0) {
    parts.push(plural(created, "new text field"));
  }
  return parts.join(" · ");
};

/**
 * The second step: the file, its columns matched to people's details and fields, a group to join,
 * and the import. Columns set to a new field get a text field each, created one after another
 * just before the upload; a created one is matched as an existing field from then on, so trying
 * again never creates it twice. Once the import is accepted, the page of the import follows it.
 */
const MatchColumns = ({
  fields,
  file,
  onReset,
}: {
  fields: readonly FieldObject[];
  file: CsvFile;
  onReset: () => void;
}) => {
  const workspace = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const groups = useQuery(groupOptionsQuery(workspace));
  const [targets, setTargets] = useState(() => guessTargets(file, fields));
  const [groupId, setGroupId] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const columns = columnsOf(file);
  // What Norbelys matches on its own, which the sentence above the columns reports, whatever the
  // person changed since.
  const guessed = guessTargets(file, fields);
  const matched = columns.filter(
    (column) => (guessed[column.index] ?? SKIP) !== SKIP
  ).length;
  const problem = blockingProblem(targets, fields);

  /**
   * Creates the new fields, each matched as an existing field once it exists; the targets to
   * upload, or `null` when a field was refused (the failure says which, in the API's words).
   */
  const createFields = async (): Promise<Target[] | null> => {
    let next = [...targets];
    if (!next.includes(NEW_FIELD)) {
      return next;
    }
    let current: FieldObject[];
    try {
      current = await queryClient.fetchQuery({
        ...fieldsQuery(workspace),
        staleTime: 0,
      });
    } catch (error) {
      setFailure(problemLine(error));
      return null;
    }
    const taken = new Set(current.map((field) => field.key));
    let refused: string | null = null;
    for (const column of columns) {
      if (refused === null && next[column.index] === NEW_FIELD) {
        const key = newFieldKey(column.name, taken);
        const label = newFieldLabel(column.name, column.index);
        taken.add(key);
        try {
          // oxlint-disable-next-line no-await-in-loop -- one after another: a refusal stops the rest
          await workspace.api.fields.create({ key, label, type: "text" });
          next = next.with(column.index, `fields.${key}`);
          setTargets(next);
        } catch (error) {
          refused = `The field “${label}” could not be created. ${problemLine(error)}`;
        }
      }
    }
    // The fields made so far are the workspace's now, whether or not every one was.
    void queryClient.invalidateQueries({ queryKey: fieldsKey(workspace) });
    if (refused !== null) {
      setFailure(refused);
      return null;
    }
    return next;
  };

  /** Sends the imported columns; on success, the import's page follows it. */
  const send = async (final: readonly Target[]) => {
    const uploaded: UploadColumn[] = columns.flatMap((column) => {
      const target = final[column.index] ?? SKIP;
      return target === SKIP ? [] : [{ index: column.index, name: target }];
    });
    const csv = uploadCsv(file.rows, uploaded);
    if (byteLength(csv) > IMPORT_BYTES_MAX) {
      setFailure(
        "The columns you import come to more than 16 MB. Split the file into smaller ones and import each."
      );
      return false;
    }
    try {
      const created = await workspace.api.imports.create(
        csv,
        groupId ? { group_id: groupId } : undefined
      );
      void queryClient.invalidateQueries({ queryKey: importsKey(workspace) });
      void navigate({
        params: { importId: created.id, slug: workspace.slug },
        search: { rows: file.rows.length },
        to: "/w/$slug/imports/$importId",
      });
      return true;
    } catch (error) {
      setFailure(problemLine(error));
      return false;
    }
  };

  const upload = async () => {
    setBusy(true);
    setFailure(null);
    const final = await createFields();
    // Once the import is accepted the page moves on, so the button stays busy until it does.
    if (!(final && (await send(final)))) {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-6">
      <div className="border-line bg-chrome flex flex-wrap items-center gap-3 rounded-sm border px-3 py-2">
        <HugeiconsIcon
          className="text-icon size-4 shrink-0"
          icon={FileImportIcon}
        />
        <span className="text-fg min-w-0 flex-1 truncate text-sm font-semibold">
          {file.name}
        </span>
        <span className="text-fg-3 text-xs">
          {plural(file.rows.length, "row")} · {plural(columns.length, "column")}
        </span>
        <Button disabled={busy} onClick={onReset} size="s" variant="tertiary">
          Choose another file
        </Button>
      </div>
      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-fg text-xl font-semibold">Columns</h2>
          <p className="text-fg-2 text-sm">
            Norbelys matched {matched} of {plural(columns.length, "column")}.
            Check where each one goes: Don&apos;t import leaves a column out,
            and New field keeps it in a field of its own.
          </p>
        </div>
        <ColumnsTable
          columns={columns}
          fields={fields}
          onChange={(index, target) =>
            setTargets((current) => current.with(index, target))
          }
          repeated={repeatedTargets(targets)}
          targets={targets}
        />
      </section>
      {groups.data && groups.data.length > 0 ? (
        <FormField
          className="max-w-[400px]"
          description="People already in your workspace join it too."
          htmlFor="import-group"
          label="Add everyone to a group"
          optional
        >
          <Select
            id="import-group"
            onChange={setGroupId}
            options={[
              { label: "No group", value: "" },
              ...groups.data.map((group) => ({
                label: group.name,
                value: group.id,
              })),
            ]}
            value={groupId}
          />
        </FormField>
      ) : null}
      {failure ? (
        <Alert variant="error">
          <HugeiconsIcon icon={Alert02Icon} />
          <AlertTitle>Not imported</AlertTitle>
          <AlertDescription>{failure}</AlertDescription>
        </Alert>
      ) : null}
      {/* The actions stay in view however many columns the file has. */}
      <div className="border-line bg-surface sticky bottom-0 z-10 flex flex-wrap items-center justify-end gap-x-4 gap-y-2 border-t py-4">
        <p
          className={cn(
            "mr-auto text-xs",
            problem ? "text-error-fg" : "text-fg-3"
          )}
        >
          {problem ?? summary(columns, targets)}
        </p>
        <Button
          nativeButton={false}
          render={
            <Link params={{ slug: workspace.slug }} to="/w/$slug/imports" />
          }
          variant="secondary"
        >
          Cancel
        </Button>
        <Button
          disabled={busy || Boolean(problem)}
          onClick={() => {
            void upload();
          }}
          variant="primary"
        >
          {busy ? <Spinner /> : null}
          Import {plural(file.rows.length, "person", "people")}
        </Button>
      </div>
    </div>
  );
};

/**
 * Importing people from a CSV file, on its own page: choose the file (read in the browser), then
 * check how its columns are matched (guessed from the headers, the workspace's fields and the
 * values) and import. The upload is a new CSV of the imported columns only, named as the API
 * reads them, sent with `imports.create` (`text/csv`, `group_id` in the query): a file has no
 * row limit there, unlike the JSON form's 1,000 people.
 */
export const ImportWizard = () => {
  const workspace = useWorkspace();
  const fields = useQuery(fieldsQuery(workspace));
  // Read while the file is chosen, so the matching opens with its group choice in place.
  useQuery(groupOptionsQuery(workspace));
  const [file, setFile] = useState<CsvFile | null>(null);
  if (!file) {
    return <ChooseFile onRead={setFile} />;
  }
  if (fields.isError) {
    return (
      <ProblemPanel
        error={fields.error}
        onRetry={() => {
          void fields.refetch();
        }}
      />
    );
  }
  if (!fields.data) {
    return <Spinner className="text-fg-3 mx-auto my-16 size-6" />;
  }
  return (
    <MatchColumns
      fields={fields.data}
      file={file}
      onReset={() => setFile(null)}
    />
  );
};
