import { Cancel01Icon, Search01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { Button } from "@/components/ui/button";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";

/** The search field above a list, with a clear button and Escape to clear it. */
export const SearchInput = ({
  className,
  describedBy,
  label,
  maxLength,
  onChange,
  placeholder = "Search...",
  value,
}: {
  className?: string;
  describedBy?: string;
  /** The field's accessible name, such as "Search people". */
  label: string;
  maxLength?: number;
  onChange: (value: string) => void;
  placeholder?: string;
  value: string;
}) => (
  <InputGroup className={className}>
    <InputGroupAddon>
      <HugeiconsIcon icon={Search01Icon} />
    </InputGroupAddon>
    <InputGroupInput
      aria-describedby={describedBy}
      aria-label={label}
      maxLength={maxLength}
      onChange={(event) => onChange(event.target.value)}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          onChange("");
        }
      }}
      placeholder={placeholder}
      type="search"
      value={value}
    />
    {value ? (
      <InputGroupAddon align="inline-end">
        <Button
          aria-label={`Clear ${label.toLowerCase()}`}
          onClick={() => onChange("")}
          size="icon-s"
          variant="tertiary"
        >
          <HugeiconsIcon icon={Cancel01Icon} />
        </Button>
      </InputGroupAddon>
    ) : null}
  </InputGroup>
);
