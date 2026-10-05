import { MoreHorizontalIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ReactNode } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { copyText } from "@/lib/actions";
import { humanize } from "@/lib/format";

/** The `⋯` button at the end of a row (30px, its icon 16px) and the row's actions. */
export const RowMenu = ({
  children,
  label = "Actions",
}: {
  children: ReactNode;
  label?: string;
}) => (
  <DropdownMenu>
    <DropdownMenuTrigger
      aria-label={label}
      className="text-icon hover:bg-hover data-popup-open:bg-hover focus-visible:outline-focus ml-auto flex size-[30px] cursor-pointer items-center justify-center rounded-sm transition-colors outline-none focus-visible:outline-1"
    >
      <HugeiconsIcon className="size-4" icon={MoreHorizontalIcon} />
    </DropdownMenuTrigger>
    <DropdownMenuContent align="end" className="w-52">
      <DropdownMenuGroup>{children}</DropdownMenuGroup>
    </DropdownMenuContent>
  </DropdownMenu>
);

/** The item that copies an id: `Copy campaign ID` for `noun` "campaign", then `Campaign ID copied`. */
export const CopyIdItem = ({ id, noun }: { id: string; noun: string }) => (
  <DropdownMenuItem onClick={() => copyText(id, `${humanize(noun)} ID copied`)}>
    Copy {noun} ID
  </DropdownMenuItem>
);
