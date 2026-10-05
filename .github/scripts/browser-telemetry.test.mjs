import { expect, spyOn, test } from "bun:test";

import {
  requestRoute,
  telemetryConfig,
} from "../../apps/app/src/lib/telemetry-policy.ts";
import { project, telemetry } from "../../apps/app/worker/telemetry.ts";

const env = {
  OTEL_EXPORTER_OTLP_ENDPOINT: "https://collector.example.com",
  SIGNOZ_INGESTION_KEY: "fixture-only",
  PRODUCT_RELEASE: "a".repeat(40),
  NORBELYS_TRACE_SAMPLE_PERCENT: "90",
};
const request = (body, origin = "https://app.example.com") =>
  new Request("https://app.example.com/__telemetry/v1/logs", {
    body,
    method: "POST",
    headers: {
      origin,
      "content-type": "application/json",
      "cf-connecting-ip": crypto.randomUUID(),
    },
  });

const get = () => new Request("https://app.example.com/__telemetry/config");

test("dashboard telemetry is optional and configuration contains no provider credential", async () => {
  const disabled = await telemetry(get(), {});
  expect(await disabled.json()).toEqual({ enabled: false });
  expect(disabled.headers.get("cache-control")).toBe("no-store");
  const response = await telemetry(get(), env);
  const config = await response.json();
  expect(config).toEqual({
    enabled: true,
    endpoint: "/__telemetry",
    samplePercent: 90,
    release: env.PRODUCT_RELEASE,
  });
  expect(telemetryConfig(config)?.samplePercent).toBe(90);
  expect(JSON.stringify(config)).not.toContain(env.SIGNOZ_INGESTION_KEY);
  expect(JSON.stringify(config)).not.toContain(env.OTEL_EXPORTER_OTLP_ENDPOINT);
  expect(telemetryConfig({ ...config, samplePercent: 101 })).toBeUndefined();
  expect(
    telemetryConfig({ ...config, endpoint: "https://foreign.example.com" })
  ).toBeUndefined();
  expect(
    requestRoute(
      "/api/v1/messages/msg_private?token=secret#private",
      "https://app.example.com",
      ["/v1/messages/{id}"]
    )
  ).toBe("/v1/messages/{id}");
  expect(
    requestRoute(
      "https://foreign.example.com/private",
      "https://app.example.com",
      []
    )
  ).toBe("external");
});

test("OTLP relay rejects foreign, oversized and malformed input and supplies its resource identity", async () => {
  const network = spyOn(globalThis, "fetch").mockResolvedValue(
    new Response(null, { status: 200 })
  );
  const body = {
    resourceLogs: [
      {
        resource: {
          attributes: [
            { key: "service.name", value: { stringValue: "forged-api" } },
          ],
        },
        scopeLogs: [
          { logRecords: [{ body: { stringValue: "browser.error" } }] },
        ],
      },
    ],
  };
  try {
    await telemetry(request(JSON.stringify(body)), {});
    await telemetry(
      request(JSON.stringify(body), "https://foreign.example.com"),
      env
    );
    await telemetry(request("{"), env);
    await telemetry(request("x".repeat(262_145)), env);
    expect(network).not.toHaveBeenCalled();
    await telemetry(request(JSON.stringify(body)), env);
    expect(network).toHaveBeenCalledTimes(1);
    const [[target, options]] = network.mock.calls;
    expect(String(target)).toBe("https://collector.example.com/v1/logs");
    expect(options.headers["signoz-ingestion-key"]).toBe(
      env.SIGNOZ_INGESTION_KEY
    );
    expect(options.redirect).toBe("manual");
    expect(options.body).not.toContain("forged-api");
    expect(options.body).toContain("norbelys-dashboard");
    expect(project({}, "logs", "development")).toBeUndefined();
  } finally {
    network.mockRestore();
  }
});
