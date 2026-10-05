import { createFileRoute, Outlet, useMatchRoute } from "@tanstack/react-router";

import { PageBody, PageHeader } from "@/components/page";
import { TabLinks } from "@/components/ui/tabs";
import { ExportButton } from "@/features/imports/export-dialog";
import { ImportButton } from "@/features/imports/import-button";
import { canWrite, useWorkspace } from "@/lib/workspace";

/** The tab's primary action: import a file, or start an export. */
const HeaderAction = ({ exports }: { exports: boolean }) => {
  const workspace = useWorkspace();
  if (exports) {
    return <ExportButton />;
  }
  return canWrite(workspace) ? <ImportButton /> : null;
};

/**
 * Imports and exports: the page's header with the current tab's action, the two tabs as routes
 * (`/imports` and `/imports/exports`), and the tab's list under them.
 */
const ImportsLayout = () => {
  const workspace = useWorkspace();
  const matchRoute = useMatchRoute();
  const params = { slug: workspace.slug };
  const exports = Boolean(
    matchRoute({ params, to: "/w/$slug/imports/exports" })
  );
  return (
    <PageBody>
      <PageHeader
        actions={<HeaderAction exports={exports} />}
        title="Imports and exports"
      />
      <TabLinks
        tabs={[
          {
            exact: true,
            label: "Imports",
            link: { params, to: "/w/$slug/imports" },
          },
          {
            label: "Exports",
            link: { params, to: "/w/$slug/imports/exports" },
          },
        ]}
      />
      <div className="pt-5">
        <Outlet />
      </div>
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/imports")({
  component: ImportsLayout,
});
