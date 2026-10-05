import { copyFile } from "node:fs/promises";
import path from "node:path";

const docs = path.resolve(import.meta.dirname, "..");
await copyFile(
  path.join(docs, "../../crates/server/openapi.json"),
  path.join(docs, "openapi.json")
);
process.stdout.write("Synced openapi.json from the API contract.\n");
