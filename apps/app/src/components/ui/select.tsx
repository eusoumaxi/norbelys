import { ArrowDown01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Select as SelectPrimitive } from "@base-ui/react/select";
import { cn } from "cn";

export interface SelectOption {
  label: string;
  value: string;
}

/**
 * A choice among a few values, drawn as the console's form controls (32px, 3px corners) with its
 * list as one of its menus. `value` is the chosen option's value; `null`, or a value no option
 * has, shows the placeholder.
 *
 * The trigger writes the chosen option's label itself: Base UI reads an empty string as "nothing
 * chosen" and would show the placeholder instead of an option whose value is `""` ("No group",
 * "Any segment", "Not set").
 */
function Select({
  className,
  disabled,
  id,
  label,
  onChange,
  options,
  placeholder = "Select…",
  value,
}: {
  className?: string;
  disabled?: boolean;
  id?: string;
  /** The accessible name when no visible label points at `id`. */
  label?: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  value: string | null;
}) {
  const chosen = options.find((option) => option.value === value);
  return (
    <SelectPrimitive.Root
      disabled={disabled}
      items={options}
      onValueChange={(next) => {
        if (typeof next === "string") {
          onChange(next);
        }
      }}
      value={value}
    >
      <SelectPrimitive.Trigger
        aria-label={label}
        className={cn(
          "border-field-line bg-field text-fg hover:border-line-strong data-popup-open:border-focus flex h-8 w-full min-w-0 cursor-pointer items-center justify-between gap-2 rounded-sm border px-3 text-left text-sm transition-colors outline-none focus-visible:border-focus data-disabled:cursor-not-allowed data-disabled:text-fg-4",
          className
        )}
        id={id}
      >
        <SelectPrimitive.Value
          className={cn("min-w-0 truncate", chosen ? null : "text-fg-3")}
        >
          {chosen ? chosen.label : placeholder}
        </SelectPrimitive.Value>
        <SelectPrimitive.Icon className="text-icon flex shrink-0">
          <HugeiconsIcon className="size-4" icon={ArrowDown01Icon} />
        </SelectPrimitive.Icon>
      </SelectPrimitive.Trigger>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Positioner
          alignItemWithTrigger={false}
          className="z-50 outline-none"
          sideOffset={4}
        >
          <SelectPrimitive.Popup className="border-line bg-surface shadow-menu max-h-(--available-height) min-w-(--anchor-width) overflow-y-auto rounded-sm border py-1 outline-none">
            <SelectPrimitive.List>
              {options.map((option) => (
                <SelectPrimitive.Item
                  className="text-fg data-highlighted:bg-hover flex h-8 cursor-pointer items-center gap-2 px-3 text-sm outline-none select-none"
                  key={option.value}
                  value={option.value}
                >
                  <SelectPrimitive.ItemText className="min-w-0 flex-1 truncate">
                    {option.label}
                  </SelectPrimitive.ItemText>
                  <SelectPrimitive.ItemIndicator className="text-accent flex">
                    <HugeiconsIcon className="size-4" icon={Tick02Icon} />
                  </SelectPrimitive.ItemIndicator>
                </SelectPrimitive.Item>
              ))}
            </SelectPrimitive.List>
          </SelectPrimitive.Popup>
        </SelectPrimitive.Positioner>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  );
}

export { Select };
