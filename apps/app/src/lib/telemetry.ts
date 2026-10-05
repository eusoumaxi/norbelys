import { telemetryConfig } from "./telemetry-policy";

/** Errors use OpenTelemetry logs with finite categories and a bounded emission budget. */
type Kind =
  | "browser_error"
  | "promise_rejection"
  | "render_error"
  | "route_error";
let emit: ((kind: Kind) => void) | undefined;
const pending = new Set<Kind>();
const last = new Map<Kind, number>();
let configured = false;

export const reportBrowserError = (kind: Kind): void => {
  if (!configured) {
    pending.add(kind);
    return;
  }
  const now = Date.now();
  if (!emit || now - (last.get(kind) ?? 0) < 60_000) {
    return;
  }
  last.set(kind, now);
  try {
    emit(kind);
  } catch {
    // A diagnostic failure must not recursively become a browser error.
  }
};

/** Initialize asynchronously without delaying dashboard rendering. Error categories raised
 * during configuration are buffered in a finite set and replayed after activation.
 * The SDK is loaded only after a valid opt-in. Configuration/export failure leaves the
 * product operational; error hooks never read rejection values or exception messages. */
export const initializeTelemetry = async (): Promise<void> => {
  window.addEventListener("error", () => reportBrowserError("browser_error"));
  window.addEventListener("unhandledrejection", () =>
    reportBrowserError("promise_rejection")
  );
  try {
    const response = await fetch("/__telemetry/config", {
      credentials: "omit",
      signal: AbortSignal.timeout(3000),
    });
    const config = telemetryConfig(
      response.ok ? ((await response.json()) as unknown) : null
    );
    if (config) {
      const { startInstrumentation } = await import("./instrumentation");
      emit = startInstrumentation(config);
    }
  } catch {
    // Observability availability never controls product availability.
  } finally {
    configured = true;
    for (const kind of pending) {
      reportBrowserError(kind);
    }
    pending.clear();
  }
};
