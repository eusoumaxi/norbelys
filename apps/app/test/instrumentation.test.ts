import { expect, test } from "bun:test";

import { context, trace } from "@opentelemetry/api";
import {
  BasicTracerProvider,
  InMemorySpanExporter,
  SimpleSpanProcessor,
} from "@opentelemetry/sdk-trace-base";

import { safeSpan } from "../src/lib/instrumentation";

test("privacy projection retains real SDK trace ancestry and timing without diagnostic payloads", async () => {
  const output = new InMemorySpanExporter();
  const provider = new BasicTracerProvider({
    spanProcessors: [new SimpleSpanProcessor(output)],
  });
  try {
    const tracer = provider.getTracer("fixture");
    const parent = tracer.startSpan("parent");
    const child = tracer.startSpan(
      "private label",
      {},
      trace.setSpan(context.active(), parent)
    );
    child.setAttributes({
      "http.method": "GET",
      "http.url":
        "https://app.example.com/api/v1/messages/private-id?token=secret",
      "http.status_code": 503,
      private: "secret",
    });
    child.recordException(new Error("private exception"));
    child.end();
    parent.end();
    await provider.forceFlush();
    const [actual] = output.getFinishedSpans();
    if (!actual) {
      throw new Error("The SDK did not export its completed span.");
    }
    const projected = safeSpan(actual, "https://app.example.com", [
      "/v1/messages/{id}",
    ]);
    expect(projected.spanContext()).toEqual(child.spanContext());
    expect(projected.parentSpanContext?.spanId).toBe(
      parent.spanContext().spanId
    );
    expect(projected.resource).toBe(actual.resource);
    expect(projected.instrumentationScope).toEqual(actual.instrumentationScope);
    expect(projected.duration).toEqual(actual.duration);
    expect(projected.ended).toBe(true);
    expect(projected.name).toBe("GET /v1/messages/{id}");
    expect(projected.status.code).toBe(2);
    expect(projected.events).toEqual([]);
    expect(JSON.stringify(projected)).not.toContain("private");
    expect(JSON.stringify(projected)).not.toContain("secret");
  } finally {
    await provider.shutdown();
  }
});
