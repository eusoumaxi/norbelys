import { Link } from "@tanstack/react-router";

import { Illustration } from "@/components/illustration";
import { Button } from "@/components/ui/button";
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyTitle,
} from "@/components/ui/empty";

/** An address that leads nowhere, or to a workspace this person is not a member of. */
export const NotFound = () => (
  <div className="bg-surface grid min-h-full place-items-center p-6">
    <Empty>
      <Illustration className="mb-2" name="not-found" />
      <EmptyHeader>
        <EmptyTitle>Page not found</EmptyTitle>
        <EmptyDescription>
          This address doesn’t exist, or it belongs to a workspace you are not a
          member of.
        </EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button render={<Link to="/workspaces" />} variant="secondary">
          Your workspaces
        </Button>
      </EmptyContent>
    </Empty>
  </div>
);
