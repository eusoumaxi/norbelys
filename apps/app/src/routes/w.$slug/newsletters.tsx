import { createFileRoute } from "@tanstack/react-router";

import { NewsletterStudio } from "@/features/newsletters/newsletter-studio";

export const Route = createFileRoute("/w/$slug/newsletters")({
  head: () => ({ meta: [{ title: "Newsletters · Norbelys" }] }),
  component: NewsletterStudio,
});
