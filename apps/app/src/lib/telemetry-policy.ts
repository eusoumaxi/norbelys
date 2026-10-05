/** Runtime configuration contains no provider credential or destination. The serving
 * deployment exposes only its own relay; an absent or invalid configuration is disabled. */
export interface TelemetryConfig {
  endpoint: "/__telemetry";
  samplePercent: number;
  release: string;
}

export const telemetryConfig = (
  value: unknown
): TelemetryConfig | undefined => {
  if (
    !value ||
    typeof value !== "object" ||
    !("enabled" in value) ||
    value.enabled !== true ||
    !("endpoint" in value) ||
    value.endpoint !== "/__telemetry" ||
    !("samplePercent" in value) ||
    typeof value.samplePercent !== "number" ||
    !Number.isInteger(value.samplePercent) ||
    value.samplePercent < 0 ||
    value.samplePercent > 100 ||
    !("release" in value) ||
    typeof value.release !== "string" ||
    !/^(?:[a-f0-9]{40}|development)$/u.test(value.release)
  ) {
    return;
  }
  return {
    endpoint: value.endpoint,
    samplePercent: value.samplePercent,
    release: value.release,
  };
};

/** Match the public API contract before exporting a path. Unknown routes, object identifiers,
 * search parameters and fragments never become telemetry attributes. */
export const requestRoute = (
  raw: string,
  origin: string,
  routes: readonly string[]
): string => {
  try {
    const url = new URL(raw, origin);
    if (url.origin !== origin) {
      return "external";
    }
    if (!url.pathname.startsWith("/api/")) {
      return "dashboard";
    }
    const parts = url.pathname.slice(4).split("/");
    return (
      routes.find((route) => {
        const template = route.split("/");
        return (
          template.length === parts.length &&
          template.every((part, index) =>
            part.startsWith("{") ? Boolean(parts[index]) : part === parts[index]
          )
        );
      }) ?? "unmatched"
    );
  } catch {
    return "unmatched";
  }
};
