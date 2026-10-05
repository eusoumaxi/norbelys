import { Add01Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import type { Provider } from "@norbelys/sdk";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { providerScopesQuery } from "@/features/mailboxes/queries";
import { QuotaScopeDialog } from "@/features/mailboxes/quota-scope-dialog";
import { useWorkspace } from "@/lib/workspace";

/** The choice that names no scope; the API takes none (or `null`) for it. */
const NONE = "none";

/**
 * A choice among the workspace's quota scopes of one provider, and a button that creates one
 * (with the provider fixed) and picks it. `value` is a scope id, or `""` for none, which only a
 * connection that may go without one (`required` false) offers.
 */
export const ScopeSelect = ({
  disabled = false,
  id,
  onChange,
  provider,
  required = false,
  value,
}: {
  disabled?: boolean;
  id?: string;
  onChange: (value: string) => void;
  provider: Provider;
  required?: boolean;
  value: string;
}) => {
  const workspace = useWorkspace();
  const scopes = useQuery(providerScopesQuery(workspace, provider));
  const [creating, setCreating] = useState(false);
  const options = [
    ...(required ? [] : [{ label: "None", value: NONE }]),
    ...(scopes.data ?? []).map((scope) => ({
      label: scope.scope_key,
      value: scope.id,
    })),
  ];
  let placeholder = "Choose a provider account";
  if (scopes.isPending) {
    placeholder = "Loading…";
  } else if (options.length === 0) {
    placeholder = "None yet: add one";
  }
  const shown = value || (required ? null : NONE);
  return (
    <div className="flex gap-2">
      <Select
        className="font-mono"
        disabled={disabled}
        id={id}
        onChange={(next) => onChange(next === NONE ? "" : next)}
        options={options}
        placeholder={placeholder}
        value={shown}
      />
      <Button
        disabled={disabled}
        onClick={() => setCreating(true)}
        type="button"
        variant="secondary"
      >
        <HugeiconsIcon icon={Add01Icon} />
        Add account
      </Button>
      <QuotaScopeDialog
        onOpenChange={setCreating}
        onSaved={(scope) => onChange(scope.id)}
        open={creating}
        provider={provider}
      />
    </div>
  );
};
