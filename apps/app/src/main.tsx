import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "@/app";
import { initializeTelemetry, reportBrowserError } from "@/lib/telemetry";

import "./index.css";

const root = document.querySelector("#root");

if (!root) {
  throw new Error("index.html has no #root element.");
}

void initializeTelemetry();
createRoot(root, {
  onCaughtError: () => reportBrowserError("render_error"),
  onRecoverableError: () => reportBrowserError("render_error"),
  onUncaughtError: () => reportBrowserError("render_error"),
}).render(
  <StrictMode>
    <App />
  </StrictMode>
);
