import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";

import { PageBody, PageHeader } from "@/components/page";
import { Problem } from "@/components/problem";
import { Skeleton } from "@/components/ui/skeleton";
import {
  ImportActions,
  ImportReport,
  ImportStatusLine,
} from "@/features/imports/import-report";
import { importQuery } from "@/features/imports/queries";
import { useWorkspace } from "@/lib/workspace";

/** The page while the import is read for the first time. */
const ImportSkeleton = () => (
  <div className="flex max-w-[880px] flex-col gap-6">
    <Skeleton className="h-5 w-80" />
    <Skeleton className="h-24 w-full" />
  </div>
);

/**
 * One import, where a new import lands once its file is accepted and where a row of the imports
 * opens: how it is going while it runs (read again every 2 s), then what it brought in and the
 * rows it could not, with the way to the people it imported.
 */
const ImportPage = () => {
  const workspace = useWorkspace();
  const { importId } = Route.useParams();
  const { rows } = Route.useSearch();
  const read = useQuery(importQuery(workspace, importId));
  const item = read.data;
  let body = <ImportSkeleton />;
  if (item) {
    body = <ImportReport item={item} rows={rows ?? undefined} />;
  } else if (read.isError) {
    body = (
      <Problem
        error={read.error}
        onRetry={() => {
          void read.refetch();
        }}
      />
    );
  }
  return (
    <PageBody>
      <PageHeader
        actions={item ? <ImportActions item={item} /> : null}
        back={{
          label: "Imports",
          link: { params: { slug: workspace.slug }, to: "/w/$slug/imports" },
        }}
        subtitle={item ? <ImportStatusLine item={item} /> : null}
        title="Import"
      />
      {body}
    </PageBody>
  );
};

export const Route = createFileRoute("/w/$slug/imports_/$importId")({
  // `rows`: how many rows the uploaded file holds, set by the import of a file that lands here.
  validateSearch: z.object({
    rows: z.preprocess(
      (value) =>
        typeof value === "number" && Number.isInteger(value) && value > 0
          ? value
          : null,
      z.number().nullable().optional()
    ),
  }),
  head: () => ({ meta: [{ title: "Import · Norbelys" }] }),
  component: ImportPage,
});
