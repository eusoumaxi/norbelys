import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { ThemeProvider } from "next-themes";

import { queryClient, router } from "@/router";

/**
 * The dashboard: signed in with the API's own sessions (an email code or a passkey), never a
 * third-party identity service. The root route loads the person (`GET /v1/me`) and every
 * signed-in route reads the session from the router's context.
 */
export const App = () => (
  <ThemeProvider
    attribute="class"
    defaultTheme="dark"
    disableTransitionOnChange
    enableSystem
    themes={["light", "dark"]}
  >
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </ThemeProvider>
);
