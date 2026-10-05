// The dashboard calls its own origin: /api/* is forwarded to API_ORIGIN without the prefix.
// Docker does the same with apps/app/Caddyfile, and `vite dev` runs this Worker locally.
import { telemetry } from "./telemetry.js";

const unreachable = (requestId: string): Response =>
  // RFC 9457, like the API's own errors, so the dashboard can explain it.
  Response.json(
    {
      detail: "The API did not answer. Start it, or check API_ORIGIN.",
      status: 502,
      request_id: requestId,
      title: "The API is unreachable",
      type: "about:blank",
    },
    {
      headers: {
        "content-type": "application/problem+json",
        "x-request-id": requestId,
      },
      status: 502,
    }
  );

// Hosts that only ever mean "this machine": a dashboard served from one is a development server.
const LOOPBACK = /^(?<host>localhost|127\.0\.0\.1|\[::1\]|.+\.localhost)$/u;

/**
 * The `Origin` to send upstream. The API accepts sign-in and cookie-authorised changes only from
 * the dashboard's origins (DASHBOARD_ORIGINS on the API), so a development server on this machine
 * that talks to a deployed API presents DASHBOARD_ORIGIN instead of its own. It vouches only for
 * requests its own pages made (the browser's `Origin` is this server's origin, which no other
 * site can forge); anything else goes upstream unchanged and the API refuses it as before. A
 * deployed dashboard is never on a loopback host, so this never applies to it.
 */
const upstreamOrigin = (
  incoming: URL,
  origin: string | null,
  dashboardOrigin: string | undefined
): string | null => {
  if (
    dashboardOrigin &&
    origin === incoming.origin &&
    LOOPBACK.test(incoming.hostname)
  ) {
    return dashboardOrigin;
  }
  return origin;
};

const proxyApi = async (request: Request, env: Env): Promise<Response> => {
  const incoming = new URL(request.url);
  const upstream = new URL(env.API_ORIGIN);
  const stripped = incoming.pathname.replace(/^\/api/u, "") || "/";
  upstream.pathname = stripped;
  upstream.search = incoming.search;

  const headers = new Headers(request.headers);
  headers.delete("host");
  headers.delete("x-forwarded-for");
  headers.delete("forwarded");
  const requestId = crypto.randomUUID();
  headers.set("x-request-id", requestId);
  const started = performance.now();
  const origin = upstreamOrigin(
    incoming,
    request.headers.get("origin"),
    env.DASHBOARD_ORIGIN || undefined
  );
  if (origin) {
    headers.set("origin", origin);
  }

  try {
    return await fetch(upstream, {
      body:
        request.method === "GET" || request.method === "HEAD"
          ? undefined
          : request.body,
      headers,
      method: request.method,
      redirect: "manual",
    });
  } catch {
    console.error(
      JSON.stringify({
        event: "edge.request",
        request_id: requestId,
        method: request.method,
        route: "api.proxy",
        status: 502,
        error_code: "upstream_unreachable",
        duration_ms: Math.round(performance.now() - started),
      })
    );
    return unreachable(requestId);
  }
};

export default {
  fetch(request, env) {
    if (new URL(request.url).pathname.startsWith("/__telemetry/")) {
      return telemetry(request, env);
    }
    return proxyApi(request, env);
  },
} satisfies ExportedHandler<Env>;
