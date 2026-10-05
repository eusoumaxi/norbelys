import { cn } from "cn";
import type { ReactNode } from "react";

import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import {
  describeEventType,
  EVENT_TYPE_GROUPS,
  EVENT_TYPES,
  isEventType,
} from "@/features/webhooks/event-types";
import { formatCount } from "@/lib/format";

/** One event type: its checkbox, its name in mono, and what it reports under it. */
const TypeOption = ({
  checked,
  onToggle,
  type,
}: {
  checked: boolean;
  onToggle: (checked: boolean) => void;
  type: string;
}) => (
  <Label className="text-fg hover:bg-hover cursor-pointer items-start gap-2.5 rounded-sm px-2 py-1.5 font-normal transition-colors">
    <Checkbox
      checked={checked}
      className="mt-0.5"
      onCheckedChange={(next) => onToggle(next)}
    />
    <span className="flex min-w-0 flex-col">
      <code className="text-fg font-mono text-xs font-medium">{type}</code>
      <span className="text-fg-3 text-xs">
        {describeEventType(type) ?? "A type newer than this page."}
      </span>
    </span>
  </Label>
);

/** The types of one prefix, under the console's small uppercase heading. */
const TypeGroup = ({
  children,
  label,
}: {
  children: ReactNode;
  label: string;
}) => (
  <fieldset className="flex min-w-0 flex-col gap-1">
    <legend className="text-fg-2 mb-1 px-2 text-[10px] leading-[10px] font-semibold tracking-[0.5px] uppercase">
      {label}
    </legend>
    <div className="grid gap-x-4 @xl:grid-cols-2">{children}</div>
  </fieldset>
);

/**
 * The event types an endpoint receives, as checkboxes grouped by their prefix (`message.*`,
 * `enrollment.*`) with "Select all" above them. Types the API reports but this dashboard does
 * not know yet are kept as they are, so saving never drops a subscription silently. `scroll`
 * bounds the list's height, for a dialog.
 */
export const EventTypePicker = ({
  invalid = false,
  onChange,
  scroll = false,
  value,
}: {
  invalid?: boolean;
  onChange: (next: string[]) => void;
  scroll?: boolean;
  value: readonly string[];
}) => {
  const chosen = new Set(value);
  const known = EVENT_TYPES.filter((type) => chosen.has(type));
  const unknown = value.filter((type) => !isEventType(type));
  const toggle = (type: string, checked: boolean) => {
    const others = value.filter((existing) => existing !== type);
    onChange(checked ? [...others, type] : others);
  };
  return (
    <div
      className={cn(
        "@container flex flex-col rounded-sm border",
        invalid ? "border-error-line" : "border-line"
      )}
    >
      <div className="border-line bg-chrome flex h-10 items-center justify-between gap-3 rounded-t-sm border-b px-4">
        <Label className="text-fg cursor-pointer gap-2.5">
          <Checkbox
            checked={known.length === EVENT_TYPES.length}
            onCheckedChange={(next) =>
              onChange(next ? [...unknown, ...EVENT_TYPES] : unknown)
            }
          />
          Select all
        </Label>
        <span className="text-fg-3 text-xs tabular-nums">
          {formatCount(value.length)} of{" "}
          {formatCount(EVENT_TYPES.length + unknown.length)} selected
        </span>
      </div>
      <div
        className={cn(
          "flex flex-col gap-3 px-2 py-3",
          scroll ? "max-h-72 scrollbar-thin overflow-y-auto" : null
        )}
      >
        {EVENT_TYPE_GROUPS.map((group) => (
          <TypeGroup key={group.prefix} label={group.label}>
            {group.types.map((type) => (
              <TypeOption
                checked={chosen.has(type)}
                key={type}
                onToggle={(checked) => toggle(type, checked)}
                type={type}
              />
            ))}
          </TypeGroup>
        ))}
        {unknown.length > 0 ? (
          <TypeGroup label="Newer types">
            {unknown.map((type) => (
              <TypeOption
                checked
                key={type}
                onToggle={(checked) => toggle(type, checked)}
                type={type}
              />
            ))}
          </TypeGroup>
        ) : null}
      </div>
    </div>
  );
};
