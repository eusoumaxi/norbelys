import { FileImportIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import { Link } from "@tanstack/react-router";

import { Button } from "@/components/ui/button";
import { useWorkspace } from "@/lib/workspace";

/**
 * "Import people": opens the import page (`/imports/new`), where a CSV file is chosen, its columns
 * matched and its people imported. Primary where importing is the view's main action (the
 * imports), secondary beside another one (the people list's "Add person").
 */
export const ImportButton = ({
  variant = "primary",
}: {
  variant?: "primary" | "secondary";
}) => {
  const workspace = useWorkspace();
  return (
    <Button
      nativeButton={false}
      render={
        <Link params={{ slug: workspace.slug }} to="/w/$slug/imports/new" />
      }
      variant={variant}
    >
      <HugeiconsIcon icon={FileImportIcon} />
      Import people
    </Button>
  );
};
