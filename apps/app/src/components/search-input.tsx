import { Search01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";

/** The search field above a list: a magnifier, the text, and Escape to clear it. */
export const SearchInput = ({
  className,
  label,
  maxLength,
  onChange,
  placeholder = "Search...",
  value,
}: {
  className?: string;
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
  </InputGroup>
);
