import { Alert02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { parseAsString, useQueryStates } from "nuqs";
import { useCallback } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";

const RETURN = {
  connection_id: parseAsString,
  error: parseAsString,
  error_description: parseAsString,
};

/**
 * What the API's OAuth callback appended to the page a Google or Microsoft consent came back to:
 * the connection it created or restored (`connection_id`), or why it did not (`error`,
 * `error_description`). `clear` takes them out of the address once they have been shown.
 */
export const useConsentReturn = () => {
  const [params, setParams] = useQueryStates(RETURN);
  // Stable, so an effect that shows a landing once and then clears it does not run again.
  const clear = useCallback(() => {
    void setParams({
      connection_id: null,
      error: null,
      error_description: null,
    });
  }, [setParams]);
  return {
    clear,
    connectionId: params.connection_id,
    error: params.error
      ? {
          code: params.error,
          description:
            params.error_description ??
            "The provider did not grant the consent.",
        }
      : null,
  };
};

/** A consent that came back refused: the provider's or the API's words, and a way to dismiss them. */
export const ConsentRefused = ({
  description,
  onDismiss,
  title,
}: {
  description: string;
  onDismiss: () => void;
  title: string;
}) => (
  <Alert className="mb-5" variant="error">
    <HugeiconsIcon icon={Alert02Icon} />
    <AlertTitle>{title}</AlertTitle>
    <AlertDescription className="flex flex-col items-start gap-3">
      {description}
      <Button onClick={onDismiss} size="s" variant="secondary">
        Dismiss
      </Button>
    </AlertDescription>
  </Alert>
);
