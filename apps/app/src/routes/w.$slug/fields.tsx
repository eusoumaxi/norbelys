import {
  Add01Icon,
  Calendar03Icon,
  CheckListIcon,
  HashtagIcon,
  Note01Icon,
  TextIcon,
  ToggleOnIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { IconSvgElement } from "@hugeicons/react";
import type { FieldObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

import { Copyable } from "@/components/copy";
import { Dash, DataTable, NameCell } from "@/components/data-table";
import { PageBody, PageHeader } from "@/components/page";
import { ProblemPanel } from "@/components/problem";
import { RowMenu } from "@/components/row-menu";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { DeleteFieldDialog } from "@/features/people/delete-field";
import { FieldDialog } from "@/features/people/field-dialog";
import { fieldTypeLabel } from "@/features/people/fields";
import { fieldsQuery } from "@/features/people/queries";
import { copyText } from "@/lib/actions";
import { formatCount, formatRelative } from "@/lib/format";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** A workspace defines at most this many fields. */
const FIELDS_MAX = 100;

/** How many of an enum's options a row shows before "+N". */
const OPTIONS_SHOWN = 4;

const TYPE_ICONS: Record<string, IconSvgElement> = {
  boolean: ToggleOnIcon,
  date: Calendar03Icon,
  enum: CheckListIcon,
  number: HashtagIcon,
  text: TextIcon,
};

/** An enum's options as pills, the first few then how many more. */
const Options = ({ options }: { options: string[] }) => {
  if (options.length === 0) {
    return <Dash />;
  }
  const more = options.length - OPTIONS_SHOWN;
  return (
    <span className="flex flex-wrap items-center gap-1">
      {options.slice(0, OPTIONS_SHOWN).map((option) => (
        <Badge className="max-w-40" key={option}>
          <span className="truncate">{option}</span>
        </Badge>
      ))}
      {more > 0 ? <span className="text-fg-3 text-xs">+{more}</span> : null}
    </span>
  );
};

/** What the fields page is doing besides listing. */
type Mode = "create" | "edit" | "delete" | null;

/**
 * The workspace's custom field definitions (`fields.list`, at most 100, oldest first): the typed
 * attributes every person can hold, named by key in the API, in CSV imports and in segment
 * conditions. Writers create one, edit its label or an enum's options, and delete it.
 */
const FieldsPage = () => {
  const workspace = useWorkspace();
  const editable = canWrite(workspace);
  const fields = useQuery(fieldsQuery(workspace));
  const [mode, setMode] = useState<Mode>(null);
  // The field being edited or deleted; it stays while its dialog closes.
  const [target, setTarget] = useState<FieldObject | null>(null);
  const start = (next: Mode, field: FieldObject | null) => {
    setTarget(field);
    setMode(next);
  };
  const close = (open: boolean) => {
    if (!open) {
      setMode(null);
    }
  };
  const count = fields.data?.length ?? 0;
  const createButton = editable ? (
    <Button
      disabled={count >= FIELDS_MAX}
      onClick={() => start("create", null)}
      variant="primary"
    >
      <HugeiconsIcon icon={Add01Icon} />
      Create field
    </Button>
  ) : null;

  return (
    <PageBody>
      <PageHeader actions={createButton} title="Fields" />
      {fields.isError ? (
        <ProblemPanel
          error={fields.error}
          onRetry={() => {
            void fields.refetch();
          }}
        />
      ) : (
        <DataTable<FieldObject>
          columns={[
            {
              render: (f) => (
                <NameCell icon={TYPE_ICONS[f.type] ?? Note01Icon}>
                  {f.label}
                </NameCell>
              ),
              header: "Label",
              id: "label",
            },
            {
              render: (f) => (
                <span className="text-fg-2 text-xs">
                  <Copyable mono value={f.key} />
                </span>
              ),
              header: "Key",
              id: "key",
            },
            {
              render: (f) => fieldTypeLabel(f.type),
              header: "Type",
              id: "type",
            },
            {
              render: (f) => <Options options={f.options} />,
              header: "Options",
              id: "options",
            },
            {
              render: (f) => formatRelative(f.updated_at),
              header: "Updated",
              id: "updated",
            },
            {
              render: (f) => (
                <RowMenu label={`Actions for ${f.label}`}>
                  {editable ? (
                    <DropdownMenuItem onClick={() => start("edit", f)}>
                      Edit field
                    </DropdownMenuItem>
                  ) : null}
                  <DropdownMenuItem
                    onClick={() => copyText(f.key, "Field key copied")}
                  >
                    Copy key
                  </DropdownMenuItem>
                  {editable ? (
                    <DropdownMenuItem
                      className="text-error-fg"
                      onClick={() => start("delete", f)}
                    >
                      Delete field
                    </DropdownMenuItem>
                  ) : null}
                </RowMenu>
              ),
              className: "w-[62px]",
              header: "",
              id: "menu",
            },
          ]}
          empty={{
            action: createButton ?? undefined,
            description:
              "A custom field is a typed attribute every person can hold, such as an industry, a plan or a renewal date. Imports read it from a column named by its key; segments filter on it; campaigns use it as a variable.",
            icon: Note01Icon,
            title: "No custom fields yet",
          }}
          footer={
            count > 0 ? (
              <p className="text-fg-3 pt-3 text-xs">
                {formatCount(count)} of {FIELDS_MAX} fields. A field&apos;s key
                and type never change.
              </p>
            ) : null
          }
          loading={fields.isPending}
          onRowClick={editable ? (f) => start("edit", f) : undefined}
          rowKey={(f) => f.id}
          rows={fields.data ?? []}
        />
      )}
      <FieldDialog
        field={mode === "create" ? null : target}
        onOpenChange={close}
        open={mode === "create" || mode === "edit"}
      />
      <DeleteFieldDialog
        field={target}
        onOpenChange={close}
        open={mode === "delete"}
      />
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/fields")({
  head: () => ({ meta: [{ title: "Fields · Norbelys" }] }),
  component: FieldsPage,
});
