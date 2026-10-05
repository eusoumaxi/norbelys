import {
  BookOpen01Icon,
  ComputerIcon,
  GithubIcon,
  LinkSquare02Icon,
  Logout01Icon,
  Moon02Icon,
  Sun03Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useNavigate } from "@tanstack/react-router";
import { useTheme } from "next-themes";

import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useSession, useSignOut } from "@/lib/auth";
import { DOCS, REPOSITORY } from "@/lib/links";

const themes = [
  { icon: ComputerIcon, label: "System", value: "system" },
  { icon: Moon02Icon, label: "Dark", value: "dark" },
  { icon: Sun03Icon, label: "Light", value: "light" },
] as const;

/**
 * The top bar's account button, a round initial, and its menu: who is signed in, their settings,
 * the theme, help, and signing out. The workspace has its own menu beside the logo.
 */
export const UserMenu = () => {
  const session = useSession();
  const navigate = useNavigate();
  const signOut = useSignOut();
  const { setTheme, theme = "dark" } = useTheme();
  const { email, name } = session.me;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label="Account menu"
        className="hover:bg-hover data-popup-open:bg-hover focus-visible:outline-focus grid size-8 shrink-0 cursor-pointer place-items-center rounded-full transition-colors outline-none focus-visible:outline-1"
      >
        <Avatar className="size-6">
          <AvatarFallback className="bg-selected text-fg text-xs">
            {email.charAt(0).toUpperCase()}
          </AvatarFallback>
        </Avatar>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-60">
        <DropdownMenuGroup>
          <div className="flex flex-col px-3 py-1.5">
            <span className="text-fg truncate text-sm font-semibold">
              {name || email.split("@")[0]}
            </span>
            <span className="text-fg-3 truncate text-xs">{email}</span>
          </div>
          <DropdownMenuItem
            onClick={() => {
              void navigate({ to: "/account" });
            }}
          >
            Account settings
          </DropdownMenuItem>
        </DropdownMenuGroup>
        <DropdownMenuGroup>
          <DropdownMenuLabel>Theme</DropdownMenuLabel>
          <DropdownMenuRadioGroup
            onValueChange={(value: string) => setTheme(value)}
            value={theme}
          >
            {themes.map((option) => (
              <DropdownMenuRadioItem key={option.value} value={option.value}>
                <HugeiconsIcon icon={option.icon} />
                {option.label}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuGroup>
        <DropdownMenuGroup>
          <DropdownMenuItem
            render={
              <a href={DOCS} rel="noreferrer" target="_blank">
                <HugeiconsIcon icon={BookOpen01Icon} />
                <span className="flex-1">Documentation</span>
                <HugeiconsIcon className="size-3!" icon={LinkSquare02Icon} />
              </a>
            }
          />
          <DropdownMenuItem
            render={
              <a href={`${REPOSITORY}/issues`} rel="noreferrer" target="_blank">
                <HugeiconsIcon icon={GithubIcon} />
                <span className="flex-1">Report an issue</span>
                <HugeiconsIcon className="size-3!" icon={LinkSquare02Icon} />
              </a>
            }
          />
        </DropdownMenuGroup>
        <DropdownMenuGroup>
          <DropdownMenuItem onClick={signOut}>
            <HugeiconsIcon icon={Logout01Icon} />
            Log out
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
