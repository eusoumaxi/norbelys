import { Combobox } from "@base-ui/react/combobox";
import { ArrowDown01Icon, Cancel01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { PersonObject } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { peopleSearchQuery } from "@/features/people/queries";
import { formatAddress, formatName } from "@/lib/format";
import { useWorkspace } from "@/lib/workspace";

/** `Ada Lovelace <ada@example.com>`, or the address alone: a person as a picker names them. */
export const personLabel = (person: PersonObject): string =>
  formatAddress({ email: person.email, name: formatName(person) });

/** `value`, once it has stopped changing for `delay` milliseconds: a search sent as typing pauses. */
const useSettled = <T,>(value: T, delay: number): T => {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), delay);
    return () => clearTimeout(timer);
  }, [value, delay]);
  return settled;
};

const iconButton =
  "text-icon hover:text-fg flex size-7 shrink-0 cursor-pointer items-center justify-center rounded-sm outline-none focus-visible:outline-1 focus-visible:outline-focus [&_svg]:size-4";

/**
 * Picks one of the workspace's people: typing searches their addresses, names and companies (the
 * newest eight show before anything is typed). `null` while no one is picked.
 */
export const PersonPicker = ({
  id,
  onChange,
  placeholder = "Search people",
  value,
}: {
  id?: string;
  onChange: (person: PersonObject | null) => void;
  placeholder?: string;
  value: PersonObject | null;
}) => {
  const workspace = useWorkspace();
  const [query, setQuery] = useState("");
  const search = useSettled(query.trim(), 250);
  const results = useQuery(peopleSearchQuery(workspace, search));
  const found = results.data ?? [];
  // The person picked stays a choice while other results come and go.
  const items =
    value && !found.some((person) => person.id === value.id)
      ? [value, ...found]
      : found;
  return (
    <Combobox.Root
      filter={null}
      isItemEqualToValue={(item: PersonObject, picked: PersonObject) =>
        item.id === picked.id
      }
      itemToStringLabel={personLabel}
      items={items}
      onInputValueChange={(next, { reason }) => {
        if (reason !== "item-press") {
          setQuery(next);
        }
      }}
      onValueChange={(next: PersonObject | null) => {
        onChange(next);
        setQuery("");
      }}
      value={value}
    >
      <Combobox.InputGroup className="border-field-line bg-field text-fg hover:border-line-strong has-[input:focus-visible]:border-focus flex h-8 w-full min-w-0 items-center rounded-sm border pr-0.5 text-sm transition-colors">
        <Combobox.Input
          className="placeholder:text-fg-3 h-full min-w-0 flex-1 bg-transparent px-3 outline-none"
          id={id}
          placeholder={placeholder}
        />
        {value ? (
          <Combobox.Clear aria-label="Clear" className={iconButton}>
            <HugeiconsIcon icon={Cancel01Icon} />
          </Combobox.Clear>
        ) : null}
        <Combobox.Trigger aria-label="Show people" className={iconButton}>
          <HugeiconsIcon icon={ArrowDown01Icon} />
        </Combobox.Trigger>
      </Combobox.InputGroup>
      <Combobox.Portal>
        <Combobox.Positioner className="z-50 outline-none" sideOffset={4}>
          <Combobox.Popup className="border-line bg-surface shadow-menu max-h-(--available-height) w-(--anchor-width) overflow-y-auto rounded-sm border py-1 transition-opacity duration-120 outline-none data-ending-style:opacity-0 data-starting-style:opacity-0">
            <Combobox.Empty>
              <p className="text-fg-3 px-3 py-1.5 text-sm">
                {results.isFetching ? "Searching…" : "No one matches."}
              </p>
            </Combobox.Empty>
            <Combobox.List>
              {(person: PersonObject) => (
                <Combobox.Item
                  className="data-highlighted:bg-hover flex cursor-pointer flex-col px-3 py-1.5 outline-none select-none"
                  key={person.id}
                  value={person}
                >
                  <span className="text-fg truncate text-sm">
                    {formatName(person) || person.email}
                  </span>
                  {formatName(person) ? (
                    <span className="text-fg-3 truncate text-xs">
                      {person.email}
                    </span>
                  ) : null}
                </Combobox.Item>
              )}
            </Combobox.List>
          </Combobox.Popup>
        </Combobox.Positioner>
      </Combobox.Portal>
    </Combobox.Root>
  );
};
