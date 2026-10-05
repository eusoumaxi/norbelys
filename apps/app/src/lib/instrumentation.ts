import { SpanStatusCode } from "@opentelemetry/api";
import type { Attributes } from "@opentelemetry/api";
import { SeverityNumber } from "@opentelemetry/api-logs";
import { ZoneContextManager } from "@opentelemetry/context-zone";
import { OTLPLogExporter } from "@opentelemetry/exporter-logs-otlp-http";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-http";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
import { registerInstrumentations } from "@opentelemetry/instrumentation";
import { DocumentLoadInstrumentation } from "@opentelemetry/instrumentation-document-load";
import { FetchInstrumentation } from "@opentelemetry/instrumentation-fetch";
import { UserInteractionInstrumentation } from "@opentelemetry/instrumentation-user-interaction";
import { XMLHttpRequestInstrumentation } from "@opentelemetry/instrumentation-xml-http-request";
import { resourceFromAttributes } from "@opentelemetry/resources";
import {
  BatchLogRecordProcessor,
  LoggerProvider,
} from "@opentelemetry/sdk-logs";
import {
  MeterProvider,
  PeriodicExportingMetricReader,
} from "@opentelemetry/sdk-metrics";
import {
  BatchSpanProcessor,
  ParentBasedSampler,
  TraceIdRatioBasedSampler,
} from "@opentelemetry/sdk-trace-base";
import type { ReadableSpan, SpanExporter } from "@opentelemetry/sdk-trace-base";
import { WebTracerProvider } from "@opentelemetry/sdk-trace-web";
import { onCLS, onFCP, onINP, onLCP, onTTFB } from "web-vitals";

import { requestRoute } from "./telemetry-policy";
import type { TelemetryConfig } from "./telemetry-policy";

declare const __NORBELYS_API_ROUTES__: readonly string[];

/** Automatic instrumentations may include URLs and exception text by default. Export a
 * fresh projection containing only method, contract route, numeric status and timings.
 * The original span is left intact for the SDK; no response body or browser identity is read. */
export const safeSpan = (
  span: ReadableSpan,
  origin: string,
  routes: readonly string[]
): ReadableSpan => {
  const attributes: Attributes = {};
  const method =
    span.attributes["http.request.method"] ?? span.attributes["http.method"];
  if (
    typeof method === "string" &&
    /^(?:GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD)$/u.test(method)
  ) {
    attributes["http.request.method"] = method;
  }
  const status =
    span.attributes["http.response.status_code"] ??
    span.attributes["http.status_code"];
  if (typeof status === "number") {
    attributes["http.response.status_code"] = status;
  }
  const url = span.attributes["url.full"] ?? span.attributes["http.url"];
  const route = requestRoute(
    typeof url === "string" ? url : "/",
    origin,
    routes
  );
  attributes["http.route"] = route;
  const interaction =
    span.instrumentationScope.name ===
    "@opentelemetry/instrumentation-user-interaction";
  const action =
    span.attributes["event_type"] === "submit" ? "ui.submit" : "ui.click";
  let name = "document.load";
  if (interaction) {
    name = action;
  }
  if (attributes["http.request.method"]) {
    name = `${String(attributes["http.request.method"])} ${route}`;
  }
  return {
    // SDK spans expose some fields through prototype getters; spreading the instance loses them.
    kind: span.kind,
    spanContext: () => span.spanContext(),
    parentSpanContext: span.parentSpanContext,
    startTime: span.startTime,
    endTime: span.endTime,
    duration: span.duration,
    ended: span.ended,
    resource: span.resource,
    instrumentationScope: span.instrumentationScope,
    droppedAttributesCount: span.droppedAttributesCount,
    droppedEventsCount: span.droppedEventsCount,
    droppedLinksCount: span.droppedLinksCount,
    name,
    attributes,
    events: [],
    links: [],
    status: {
      code:
        typeof status === "number" && status >= 400
          ? SpanStatusCode.ERROR
          : span.status.code,
    },
  };
};

export const startInstrumentation = (
  config: TelemetryConfig
): ((kind: string) => void) => {
  const resource = resourceFromAttributes({
    "service.name": "norbelys-dashboard",
    "service.namespace": "norbelys",
    "service.version": config.release,
  });
  const url = (signal: string): string =>
    new URL(`${config.endpoint}/v1/${signal}`, location.origin).href;
  const exporter = new OTLPTraceExporter({
    url: url("traces"),
    timeoutMillis: 3000,
  });
  const sanitized: SpanExporter = {
    // The OpenTelemetry SpanExporter contract requires a completion callback.
    // eslint-disable-next-line promise/prefer-await-to-callbacks
    export: (spans, callback) =>
      exporter.export(
        spans.map((span) =>
          safeSpan(span, location.origin, __NORBELYS_API_ROUTES__)
        ),
        callback
      ),
    shutdown: () => exporter.shutdown(),
  };
  const batch = {
    maxQueueSize: 256,
    maxExportBatchSize: 16,
    scheduledDelayMillis: 5000,
    exportTimeoutMillis: 3000,
  };
  const tracer = new WebTracerProvider({
    resource,
    sampler: new ParentBasedSampler({
      root: new TraceIdRatioBasedSampler(config.samplePercent / 100),
    }),
    spanProcessors: [new BatchSpanProcessor(sanitized, batch)],
  });
  tracer.register({ contextManager: new ZoneContextManager() });
  const ignoreUrls = [/\/__telemetry\//u];
  registerInstrumentations({
    instrumentations: [
      new FetchInstrumentation({ ignoreUrls, ignoreNetworkEvents: true }),
      new XMLHttpRequestInstrumentation({
        ignoreUrls,
        ignoreNetworkEvents: true,
      }),
      new DocumentLoadInstrumentation({ ignoreNetworkEvents: true }),
      new UserInteractionInstrumentation({ eventNames: ["click", "submit"] }),
    ],
  });
  const logger = new LoggerProvider({
    resource,
    processors: [
      new BatchLogRecordProcessor({
        exporter: new OTLPLogExporter({
          url: url("logs"),
          timeoutMillis: 3000,
        }),
        ...batch,
      }),
    ],
  });
  const meter = new MeterProvider({
    resource,
    readers: [
      new PeriodicExportingMetricReader({
        exporter: new OTLPMetricExporter({
          url: url("metrics"),
          timeoutMillis: 3000,
        }),
        exportIntervalMillis: 30_000,
        exportTimeoutMillis: 3000,
      }),
    ],
  });
  const metrics = meter.getMeter("norbelys-dashboard");
  const seconds = metrics.createHistogram(
    "norbelys_browser_web_vital_seconds",
    { unit: "s" }
  );
  const layout = metrics.createHistogram("norbelys_browser_layout_shift", {
    unit: "1",
  });
  onCLS((vital) => layout.record(vital.value));
  for (const register of [onFCP, onINP, onLCP, onTTFB]) {
    register((vital) =>
      seconds.record(vital.value / 1000, { "web_vital.name": vital.name })
    );
  }
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      void Promise.allSettled([
        tracer.forceFlush(),
        logger.forceFlush(),
        meter.forceFlush(),
      ]);
    }
  });
  const errors = logger.getLogger("norbelys-dashboard");
  return (kind) =>
    errors.emit({
      severityNumber: SeverityNumber.ERROR,
      severityText: "ERROR",
      body: "browser.error",
      attributes: { "error.type": kind },
    });
};
