import {
  Alert02Icon,
  CheckmarkCircle02Icon,
  InformationCircleIcon,
  Loading03Icon,
  MultiplicationSignCircleIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { useTheme } from "next-themes";
import { Toaster as Sonner, type ToasterProps } from "sonner";

/** Toasts drawn like the console's menus: black, a hairline, 3px corners. */
const Toaster = ({ ...props }: ToasterProps) => {
  const { resolvedTheme = "dark" } = useTheme();
  return (
    <Sonner
      className="toaster group"
      icons={{
        error: (
          <HugeiconsIcon
            className="text-error size-4"
            icon={MultiplicationSignCircleIcon}
            strokeWidth={2}
          />
        ),
        info: (
          <HugeiconsIcon
            className="text-info size-4"
            icon={InformationCircleIcon}
            strokeWidth={2}
          />
        ),
        loading: (
          <HugeiconsIcon
            className="size-4 animate-spin"
            icon={Loading03Icon}
            strokeWidth={2}
          />
        ),
        success: (
          <HugeiconsIcon
            className="text-success size-4"
            icon={CheckmarkCircle02Icon}
            strokeWidth={2}
          />
        ),
        warning: (
          <HugeiconsIcon
            className="text-warning size-4"
            icon={Alert02Icon}
            strokeWidth={2}
          />
        ),
      }}
      style={
        {
          "--border-radius": "3px",
          "--normal-bg": "var(--nb-surface)",
          "--normal-border": "var(--nb-line)",
          "--normal-text": "var(--nb-fg)",
        } as React.CSSProperties
      }
      theme={resolvedTheme as ToasterProps["theme"]}
      toastOptions={{
        classNames: {
          description: "text-fg-2!",
          title: "font-semibold!",
          toast: "shadow-menu! font-sans! text-sm!",
        },
      }}
      {...props}
    />
  );
};

export { Toaster };
