/** Same-origin OTLP/JSON relay. Provider destinations and ingestion keys are runtime secrets,
 * never browser configuration. Anonymous input has bounded size, time and rate; resource
 * identity comes from the deployment. OTLP serialization remains the SDK's responsibility. */
interface Settings {
  OTEL_EXPORTER_OTLP_ENDPOINT?: string;
  SIGNOZ_INGESTION_KEY?: string;
  NORBELYS_TRACE_SAMPLE_PERCENT?: string;
  PRODUCT_RELEASE?: string;
}
const buckets = new Map<string, { until: number; count: number }>();
const object = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
const array = (value: unknown): unknown[] =>
  Array.isArray(value) ? (value as unknown[]) : [];
const endpoint = (settings: Settings): URL | undefined => {
  try {
    const url = new URL(settings.OTEL_EXPORTER_OTLP_ENDPOINT ?? "");
    if (
      url.protocol === "https:" &&
      !url.username &&
      !url.password &&
      !url.search &&
      !url.hash
    ) {
      return url;
    }
  } catch {
    /* An unset or malformed destination disables export. */
  }
};
const admit = (request: Request): boolean => {
  const now = Date.now();
  for (const [key, bucket] of buckets) {
    if (bucket.until <= now) {
      buckets.delete(key);
    }
  }
  const source = request.headers.get("cf-connecting-ip") ?? "local";
  const bucket = buckets.get(source);
  if ((bucket?.count ?? 0) >= 60 || (!bucket && buckets.size >= 1024)) {
    return false;
  }
  buckets.set(source, {
    count: (bucket?.count ?? 0) + 1,
    until: bucket?.until ?? now + 60_000,
  });
  return true;
};

const payload = async (request: Request): Promise<unknown> => {
  if (!request.body) {
    return;
  }
  const reader = request.body.getReader();
  const parts: Uint8Array[] = [];
  let length = 0;
  let expired = false;
  const cancel = async (): Promise<void> => {
    try {
      await reader.cancel();
    } catch {
      // Disconnected input is discarded.
    }
  };
  const timer = setTimeout(() => {
    expired = true;
    void cancel();
  }, 3000);
  try {
    while (true) {
      // Bounded streaming avoids trusting Content-Length, which a caller controls.
      // eslint-disable-next-line no-await-in-loop
      const chunk = await reader.read();
      if (expired) {
        return;
      }
      if (chunk.done) {
        break;
      }
      length += chunk.value.length;
      if (length > 262_144 || parts.length >= 1024) {
        void cancel();
        return;
      }
      parts.push(chunk.value);
    }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const part of parts) {
      bytes.set(part, offset);
      offset += part.length;
    }
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  } catch {
    /* Malformed and interrupted input never reaches a diagnostic sink. */
  } finally {
    clearTimeout(timer);
    reader.releaseLock();
  }
};

/** OTLP stays in the SDK's native format. Only resource identity is replaced so browser
 * reports cannot impersonate a backend service. Privacy filtering belongs to the exporter. */
export const project = (
  input: unknown,
  signal: string,
  release: string
): unknown => {
  const keys: Record<string, string> = {
    traces: "resourceSpans",
    logs: "resourceLogs",
    metrics: "resourceMetrics",
  };
  const key = keys[signal];
  if (!key) {
    return;
  }
  const resources = array(object(input)[key]);
  if (resources.length === 0 || resources.length > 4) {
    return;
  }
  return {
    [key]: resources.map((value) => ({
      ...object(value),
      resource: {
        attributes: [
          { key: "service.name", value: { stringValue: "norbelys-dashboard" } },
          { key: "service.namespace", value: { stringValue: "norbelys" } },
          { key: "service.version", value: { stringValue: release } },
        ],
      },
    })),
  };
};

const accepts = (request: Request, incoming: URL): boolean =>
  request.method === "POST" &&
  request.headers.get("origin") === incoming.origin &&
  Boolean(
    request.headers.get("content-type")?.startsWith("application/json")
  ) &&
  admit(request);

const runtime = (
  settings: Settings
): { target: URL; release: string; samplePercent: number } | undefined => {
  const target = endpoint(settings);
  const raw = settings.NORBELYS_TRACE_SAMPLE_PERCENT ?? "100";
  const samplePercent = Number(raw);
  if (!target || !/^\d{1,3}$/u.test(raw) || samplePercent > 100) {
    return;
  }
  const release = /^[a-f0-9]{40}$/u.test(settings.PRODUCT_RELEASE ?? "")
    ? (settings.PRODUCT_RELEASE ?? "development")
    : "development";
  return { target, release, samplePercent };
};

export const telemetry = async (
  request: Request,
  settings: Settings
): Promise<Response> => {
  const incoming = new URL(request.url);
  const config = runtime(settings);
  const headers = { "cache-control": "no-store" };
  if (incoming.pathname === "/__telemetry/config" && request.method === "GET") {
    return Response.json(
      config
        ? {
            enabled: true,
            endpoint: "/__telemetry",
            samplePercent: config.samplePercent,
            release: config.release,
          }
        : { enabled: false },
      { headers }
    );
  }
  const empty = (): Response => new Response(null, { status: 204, headers });
  const signal = incoming.pathname.match(
    /^\/__telemetry\/v1\/(?<signal>traces|logs|metrics)$/u
  )?.groups?.signal;
  if (!config || !signal || !accepts(request, incoming)) {
    return empty();
  }
  const body = project(await payload(request), signal, config.release);
  if (!body) {
    return empty();
  }
  const { target } = config;
  target.pathname = `${target.pathname.replace(/\/$/u, "")}/v1/${signal}`;
  try {
    const response = await fetch(target, {
      method: "POST",
      body: JSON.stringify(body),
      headers: {
        "content-type": "application/json",
        ...(settings.SIGNOZ_INGESTION_KEY
          ? { "signoz-ingestion-key": settings.SIGNOZ_INGESTION_KEY }
          : {}),
      },
      redirect: "manual",
      signal: AbortSignal.timeout(3000),
    });
    if (!response.ok) {
      console.warn(
        JSON.stringify({
          event: "telemetry.export_failed",
          signal,
          status: response.status,
        })
      );
    }
    await response.body?.cancel();
  } catch {
    console.warn(
      JSON.stringify({
        event: "telemetry.export_failed",
        signal,
        error_code: "upstream_unreachable",
      })
    );
  }
  return empty();
};
