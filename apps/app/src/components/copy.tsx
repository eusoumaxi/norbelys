import { Copy01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "cn";
import { useEffect, useState } from "react";
import type { ReactNode } from "react";

/**
 * Copies text, and remembers for a moment (1.5 s) that it did: what a copy button shows its tick
 * by. `copy` rejects when the browser refuses the clipboard.
 */
const useCopy = () => {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) {
      return;
    }
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  const copy = async (text: string) => {
    await navigator.clipboard.writeText(text);
    setCopied(true);
  };
  return { copied, copy };
};

/** Copies `value` and shows a tick for a moment. */
export const CopyButton = ({
  className,
  label = "Copy",
  value,
}: {
  className?: string;
  label?: string;
  value: string;
}) => {
  const { copied, copy } = useCopy();
  return (
    <button
      aria-label={copied ? "Copied" : label}
      className={cn(
        "text-fg-3 hover:text-fg focus-visible:outline-focus flex size-5 shrink-0 cursor-pointer items-center justify-center rounded-xs transition-colors outline-none focus-visible:outline-1",
        className
      )}
      onClick={async () => {
        await copy(value);
      }}
      title={label}
      type="button"
    >
      <HugeiconsIcon
        className={cn("size-3.5", copied ? "text-accent" : null)}
        icon={copied ? Tick02Icon : Copy01Icon}
      />
    </button>
  );
};

/** A value with a copy button that appears on hover, as the console's ids. */
export const Copyable = ({
  children,
  mono = false,
  value,
}: {
  children?: ReactNode;
  mono?: boolean;
  value: string;
}) => (
  <span className="group/copy inline-flex max-w-full min-w-0 items-center gap-1">
    <span className={cn("truncate", mono ? "font-mono" : null)} title={value}>
      {children ?? value}
    </span>
    <CopyButton
      className="opacity-0 group-hover/copy:opacity-100 focus-visible:opacity-100"
      value={value}
    />
  </span>
);

/** A one-line command in a box on the chrome color, with its copy button: `$ npm i @norbelys/sdk`. */
export const CodeLine = ({
  className,
  prefix = "$",
  value,
}: {
  className?: string;
  prefix?: string | null;
  value: string;
}) => (
  <div
    className={cn(
      "border-line bg-chrome flex h-9 min-w-0 items-center gap-2 rounded-sm border pr-1.5 pl-3",
      className
    )}
  >
    <code className="text-fg min-w-0 flex-1 truncate font-mono text-xs font-medium">
      {prefix ? (
        <span className="text-fg-3 mr-2 select-none">{prefix}</span>
      ) : null}
      {value}
    </code>
    <CopyButton value={value} />
  </div>
);

/** A multi-line snippet in a box on the chrome color, with its copy button in the corner. */
export const CodeBlock = ({
  className,
  value,
}: {
  className?: string;
  value: string;
}) => (
  <div
    className={cn(
      "border-line bg-chrome relative min-w-0 rounded-sm border",
      className
    )}
  >
    <pre className="text-fg scrollbar-thin overflow-x-auto p-3 pr-9 font-mono text-xs leading-[17px]">
      {value}
    </pre>
    <CopyButton className="absolute top-2 right-2" value={value} />
  </div>
);
