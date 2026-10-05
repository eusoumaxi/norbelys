import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { ErrorComponentProps } from "@tanstack/react-router";
import { useRouter } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { DialogBody } from "@/components/ui/dialog";
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty";
import { Spinner } from "@/components/ui/spinner";
import { describeProblem, fieldProblems } from "@/lib/problem";

interface ProblemProps {
  error: unknown;
  onRetry?: () => void;
}

/** A failed request, in plain words, with the request id people should quote. */
export const Problem = ({ error, onRetry }: ProblemProps) => {
  const { detail, requestId, title } = describeProblem(error);
  return (
    <Empty>
      <EmptyMedia className="text-error">
        <HugeiconsIcon icon={Alert02Icon} />
      </EmptyMedia>
      <EmptyHeader>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription>{detail}</EmptyDescription>
      </EmptyHeader>
      {onRetry ? (
        <EmptyContent>
          <Button onClick={onRetry} variant="secondary">
            Try again
          </Button>
        </EmptyContent>
      ) : null}
      {requestId ? (
        <p className="text-fg-3 font-mono text-xs">Reference {requestId}</p>
      ) : null}
    </Empty>
  );
};

/** A failed read in a bordered box, where a list or a panel would have shown its content. */
export const ProblemPanel = (props: ProblemProps) => (
  <div className="border-line rounded-sm border">
    <Problem {...props} />
  </div>
);

/** The router's error screen: retrying reloads the route's data. */
export const RouteError = ({ error, reset }: ErrorComponentProps) => {
  const router = useRouter();
  return (
    <div className="grid min-h-full place-items-center p-6">
      <Problem
        error={error}
        onRetry={() => {
          reset();
          void router.invalidate();
        }}
      />
    </div>
  );
};

/**
 * A dialog's body while what it shows is read: a spinner, then the problem with a retry when the
 * read fails.
 */
export const DialogPending = ({
  query,
}: {
  query: { error: unknown; isError: boolean; refetch: () => unknown };
}) => (
  <DialogBody className="grid min-h-40 place-items-center">
    {query.isError ? (
      <Problem
        error={query.error}
        onRetry={() => {
          void query.refetch();
        }}
      />
    ) : (
      <Spinner className="text-fg-3" />
    )}
  </DialogBody>
);

/**
 * A failure in words, in an error alert: what a form or a dialog could not tie to one of its
 * fields (pass `problemLine(error)` for a request's, with its reference).
 */
export const ProblemAlert = ({ children }: { children: ReactNode }) => (
  <Alert variant="error">
    <AlertDescription className="col-span-2 col-start-1">
      {children}
    </AlertDescription>
  </Alert>
);

const NO_PROBLEMS: [string, string][] = [];

/**
 * Why a save was refused, beside its form: the API's own words with the reference to quote, and
 * the field problems no input shows (`unplaced`, as `[path, problem]`). When every problem shows on
 * its input, a line points at them instead. Nothing while no save failed.
 */
export const SaveFailure = ({
  failure,
  unplaced = NO_PROBLEMS,
}: {
  failure: unknown;
  unplaced?: [string, string][];
}) => {
  if (!failure) {
    return null;
  }
  const placedOnly =
    Object.keys(fieldProblems(failure)).length > 0 && unplaced.length === 0;
  const { detail, requestId } = describeProblem(failure);
  return (
    <Alert variant="error">
      <HugeiconsIcon icon={Alert02Icon} />
      <AlertTitle>Not saved</AlertTitle>
      <AlertDescription>
        {placedOnly
          ? "Some fields need attention: see the messages beside them."
          : detail}
        {unplaced.length > 0 ? (
          <ul className="mt-2 list-disc pl-4">
            {unplaced.map(([path, problem]) => (
              <li key={path}>
                <code className="font-mono text-xs">{path}</code>: {problem}
              </li>
            ))}
          </ul>
        ) : null}
        {requestId ? (
          <span className="text-fg-3 mt-1 block font-mono text-xs">
            Reference {requestId}
          </span>
        ) : null}
      </AlertDescription>
    </Alert>
  );
};
