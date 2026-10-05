/** Read-only CLI distribution. Terraform binds a private R2 bucket and uploads this module.
 * GitHub release archives retain immutable URLs. Full downloads use the edge cache; byte
 * ranges and conditional reads reach R2, so resumed downloads retain HTTP validators.
 */
export interface Env {
  RELEASES: Releases;
}

/** The read-only subset of R2 the download service uses. R2 supplies byte streams;
 * local wire tests can supply byte arrays through the same explicit body contract. */
export interface AssetMetadata {
  size: number;
  httpEtag: string;
  uploaded: Date;
  range?: R2Range;
  writeHttpMetadata: (headers: Headers) => void;
}
interface Asset extends AssetMetadata {
  body: ReadableStream<Uint8Array> | Uint8Array<ArrayBuffer> | ArrayBuffer;
}
export interface Releases {
  head: (key: string) => Promise<AssetMetadata | null>;
  get: (
    key: string,
    options?: { onlyIf?: Headers; range?: Headers }
  ) => Promise<AssetMetadata | Asset | null>;
}
const MUTABLE = new Set(["install.sh", "install.ps1", "latest.json"]);
const PAGE = `<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Norbelys CLI</title><style>body{font:18px system-ui;max-width:760px;margin:10vh auto;padding:24px;color:#17202a}pre{background:#f3f5f7;padding:20px;overflow:auto;border-radius:8px}a{color:#275dad}li{margin:12px 0}</style><main><h1>Norbelys CLI</h1><p>Work with the Norbelys API from your terminal.</p><h2>macOS and Linux</h2><pre>curl --proto '=https' --tlsv1.2 -LsSf https://cli.norbelys.com/install.sh | sh</pre><h2>Windows PowerShell</h2><pre>irm https://cli.norbelys.com/install.ps1 | iex</pre><p><a href="/install.sh">Inspect the shell installer</a> · <a href="/install.ps1">Inspect the PowerShell installer</a></p><h2>Downloads</h2><p id="status">Loading available releases…</p><ul id="downloads"></ul><script type="module">try{const r=await fetch('/latest.json');if(!r.ok)throw Error();const release=await r.json();document.querySelector('#status').textContent='Version '+release.version;for(const asset of release.assets){const li=document.createElement('li');const a=document.createElement('a');a.href=asset.url;a.textContent=asset.name;li.append(a);document.querySelector('#downloads').append(li)}}catch{document.querySelector('#status').textContent='The first release is being prepared.'}</script></main></html>`;

/** Reject encoded separators, private objects and unversioned release paths. */
export const objectKey = (path: string): string | undefined => {
  const value = path.replace(/^\//u, "");
  if (MUTABLE.has(value)) {
    return value;
  }
  return /^releases\/norbelys-cli-v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\/[0-9A-Za-z][0-9A-Za-z._-]*$/u.test(
    value
  )
    ? value
    : undefined;
};
const tagMatches = (value: string, etag: string, weak: boolean): boolean =>
  value
    .split(",")
    .some(
      (item) =>
        item.trim() === "*" ||
        (weak ? item.trim().replace(/^W\//u, "") : item.trim()) === etag
    );
/** ETags take precedence over dates; HTTP dates compare at second precision. */
export const precondition = (
  headers: Headers,
  object: AssetMetadata
): 304 | 412 | undefined => {
  const match = headers.get("if-match");
  const none = headers.get("if-none-match");
  const modified = Math.floor(object.uploaded.getTime() / 1000) * 1000;
  if (match && !tagMatches(match, object.httpEtag, false)) {
    return 412;
  }
  const unmodified = headers.get("if-unmodified-since");
  if (!match && unmodified && Date.parse(unmodified) < modified) {
    return 412;
  }
  if (none && tagMatches(none, object.httpEtag, true)) {
    return 304;
  }
  const since = headers.get("if-modified-since");
  if (!none && since && Date.parse(since) >= modified) {
    return 304;
  }
  return undefined;
};
/** R2 supports one satisfiable byte range, including suffix and open-ended ranges. */
export const validRange = (range: string, size: number): boolean => {
  const match = /^bytes=(?<start>\d*)-(?<end>\d*)$/u.exec(range);
  if (!match?.groups || !size) {
    return false;
  }
  const { end, start } = match.groups;
  if (!start) {
    return Boolean(end && Number(end) > 0);
  }
  return Number(start) < size && (!end || Number(end) >= Number(start));
};
const selectedRange = (
  request: Request,
  object: AssetMetadata
): string | null => {
  const condition = request.headers.get("if-range");
  const modified = Math.floor(object.uploaded.getTime() / 1000) * 1000;
  if (
    condition &&
    condition !== object.httpEtag &&
    !(
      Number.isFinite(Date.parse(condition)) &&
      Date.parse(condition) >= modified
    )
  ) {
    return null;
  }
  return request.headers.get("range");
};
const conditionalRead = (request: Request): boolean =>
  [
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
  ].some((name) => request.headers.has(name));
const missing = (): Response =>
  new Response("Release asset not found", {
    status: 404,
    headers: { "cache-control": "no-store" },
  });
/** Preflight validators/ranges for HEAD; R2 applies validators again during GET atomically. */
const readObject = async (
  request: Request,
  bucket: Releases,
  key: string
): Promise<AssetMetadata | Asset | Response | null> => {
  if (
    request.method === "GET" &&
    !conditionalRead(request) &&
    !request.headers.has("range")
  ) {
    return bucket.get(key);
  }
  const metadata = await bucket.head(key);
  if (!metadata) {
    return null;
  }
  const status = precondition(request.headers, metadata);
  if (status) {
    return new Response(null, { status, headers: { etag: metadata.httpEtag } });
  }
  const range = selectedRange(request, metadata);
  if (range && !validRange(range, metadata.size)) {
    return new Response(null, {
      status: 416,
      headers: { "content-range": `bytes */${metadata.size}` },
    });
  }
  if (request.method === "HEAD") {
    return metadata;
  }
  const rangeHeaders = new Headers({ range: range ?? "" });
  return bucket.get(key, {
    onlyIf: request.headers,
    ...(range ? { range: rangeHeaders } : {}),
  });
};
const contentType = (key: string): string => {
  if (key.endsWith(".json")) {
    return "application/json";
  }
  if (/\.(?:sh|ps1|sha256)$/u.test(key)) {
    return "text/plain; charset=utf-8";
  }
  return "application/octet-stream";
};
/** Preserve stream size, metadata and validators; mutable promotion files revalidate quickly. */
const objectResponse = (
  request: Request,
  key: string,
  object: AssetMetadata | Asset
): Response => {
  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("etag", object.httpEtag);
  headers.set("last-modified", object.uploaded.toUTCString());
  headers.set("accept-ranges", "bytes");
  headers.set("x-content-type-options", "nosniff");
  headers.set(
    "cache-control",
    key.startsWith("releases/")
      ? "public, max-age=31536000, immutable"
      : "public, max-age=60, must-revalidate"
  );
  if (!headers.has("content-type")) {
    headers.set("content-type", contentType(key));
  }
  if (request.method === "HEAD") {
    headers.set("content-length", String(object.size));
    return new Response(null, { headers });
  }
  if (!("body" in object)) {
    return new Response(null, {
      status: precondition(request.headers, object) ?? 412,
      headers,
    });
  }
  const { range } = object;
  const offset =
    range && "offset" in range
      ? (range.offset ?? 0)
      : Math.max(
          0,
          object.size -
            (range && "suffix" in range ? range.suffix : object.size)
        );
  const length =
    range && "length" in range
      ? (range.length ?? object.size - offset)
      : object.size - offset;
  if (range) {
    headers.set(
      "content-range",
      `bytes ${offset}-${offset + length - 1}/${object.size}`
    );
  }
  headers.set("content-length", String(range ? length : object.size));
  return new Response(object.body, { status: range ? 206 : 200, headers });
};
/** Serve GET/HEAD only; no endpoint exposes uploads, bucket listing or private objects. */
export const download = async (
  request: Request,
  env: Env,
  context: Pick<ExecutionContext, "waitUntil">
): Promise<Response> => {
  if (!["GET", "HEAD"].includes(request.method)) {
    return new Response("Method not allowed", {
      status: 405,
      headers: { allow: "GET, HEAD" },
    });
  }
  const requestId = crypto.randomUUID();
  const url = new URL(request.url);
  if (url.pathname === "/") {
    return new Response(request.method === "HEAD" ? null : PAGE, {
      headers: {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "public, max-age=300",
        "x-content-type-options": "nosniff",
      },
    });
  }
  const key = objectKey(url.pathname);
  if (!key) {
    return missing();
  }
  const cacheable =
    request.method === "GET" &&
    key.startsWith("releases/") &&
    !conditionalRead(request) &&
    !request.headers.has("range");
  const cache = typeof caches === "undefined" ? undefined : caches.default;
  const cacheRequest = new Request(`${url.origin}/${key}`);
  const cacheFailure = (operation: "read" | "write") => {
    console.error(
      JSON.stringify({
        event: "edge.cache",
        request_id: requestId,
        route: "cli.download",
        error_code: "cache_unavailable",
        operation,
      })
    );
  };
  if (cacheable && cache) {
    try {
      const hit = await cache.match(cacheRequest);
      if (hit) {
        return hit;
      }
    } catch {
      cacheFailure("read");
    }
  }
  let object: Awaited<ReturnType<typeof readObject>>;
  try {
    object = await readObject(request, env.RELEASES, key);
  } catch {
    console.error(
      JSON.stringify({
        event: "edge.request",
        request_id: requestId,
        route: "cli.download",
        error_code: "storage_unavailable",
        status: 503,
      })
    );
    return new Response("Download temporarily unavailable", {
      status: 503,
      headers: { "cache-control": "no-store" },
    });
  }
  if (!object) {
    return missing();
  }
  if (object instanceof Response) {
    return object;
  }
  const response = objectResponse(request, key, object);
  if (cacheable && cache && response.status === 200) {
    context.waitUntil(
      (async () => {
        try {
          await cache.put(cacheRequest, response.clone());
        } catch {
          cacheFailure("write");
        }
      })()
    );
  }
  return response;
};
export default { fetch: download } satisfies ExportedHandler<Env>;
