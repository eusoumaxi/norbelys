import { createFileRoute } from "@tanstack/react-router";

import { PageBody, PageHeader } from "@/components/page";
import { ProblemAlert } from "@/components/problem";
import { ImportWizard } from "@/features/imports/import-wizard";
import { canWrite, useWorkspace } from "@/lib/workspace";

/**
 * Importing people from a CSV file, on a page of its own rather than in a dialog: the columns of
 * a CRM export run to dozens, and a page scrolls them whole where a dialog would squeeze them into
 * a box of its own. Outside the imports' tabs (the `imports_` segment), with the way back to them.
 */
const NewImportPage = () => {
  const workspace = useWorkspace();
  return (
    <PageBody>
      <PageHeader
        back={{
          label: "Imports",
          link: { params: { slug: workspace.slug }, to: "/w/$slug/imports" },
        }}
        subtitle="Each row of the file adds a person. Someone already in your workspace is matched by email and updated: filled cells replace what they have, empty cells change nothing."
        title="Import people"
      />
      <div className="max-w-[880px]">
        {canWrite(workspace) ? (
          <ImportWizard />
        ) : (
          <ProblemAlert>
            Your role in this workspace can view people but not import them. Ask
            an owner or an admin to change it.
          </ProblemAlert>
        )}
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/imports_/new")({
  head: () => ({ meta: [{ title: "Import people · Norbelys" }] }),
  component: NewImportPage,
});
