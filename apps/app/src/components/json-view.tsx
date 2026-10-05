import { CodeBlock } from "@/components/copy";

/** Any value the API returned, as indented JSON in a code box with a copy button. */
export const JsonView = ({
  className,
  value,
}: {
  className?: string;
  value: unknown;
}) => (
  <CodeBlock className={className} value={JSON.stringify(value, null, 2)} />
);
