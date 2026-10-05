import { APIConnectionError, APIError, Norbelys } from "@norbelys/sdk";
import type { WorkspaceMode } from "@norbelys/sdk";

/**
 * The dashboard's own client: the browser session and the dashboard surface of the API.
 *
 * The public SDK covers what an API key can do. A person in a browser does more: signs in (an
 * email code or a passkey), holds a session cookie (`__Host-nb_session`, `HttpOnly`, so no script
 * reads it), and exchanges it for short-lived workspace tokens (`nbs_`, five minutes) that the SDK
 * sends as bearer tokens. The session operations need the cookie, the session's CSRF token in
 * `X-CSRF-Token` and an `Origin` on the API's allow-list; the dashboard calls the API on its own
 * origin under `/api` (worker/index.ts), which keeps the cookie first-party.
 */

const BASE = "/api";

export type MembershipRole = "owner" | "admin" | "member" | "viewer";
type AuthMethod =
  | "email_code"
  | "passkey"
  | "oidc"
  | "sso"
  | "break_glass"
  | "impersonation";

interface WorkspaceSummary {
  id: string;
  slug: string;
  name: string;
  mode: WorkspaceMode;
}

export interface Membership {
  id: string;
  workspace: WorkspaceSummary;
  role: MembershipRole;
  status: "active" | "suspended" | "removed";
  created_at: string;
}

export interface SessionInfo {
  id: string;
  current: boolean;
  auth_method: AuthMethod;
  authenticated_at: string;
  created_at: string;
  last_seen_at: string;
  expires_at: string;
  user_agent?: string | null;
}

export interface Passkey {
  id: string;
  name: string;
  created_at: string;
  last_used_at?: string | null;
}

export interface Grant {
  id: string;
  client_id: string;
  client_name: string;
  workspace_id: string;
  scopes: string[];
  created_at: string;
  expires_at: string;
  last_used_at?: string | null;
}

export interface Me {
  id: string;
  email: string;
  name?: string | null;
  locale: string;
  email_verified_at?: string | null;
  created_at: string;
  updated_at: string;
  version: number;
  session_id: string;
  csrf_token: string;
  sessions: SessionInfo[];
  passkeys: Passkey[];
  grants: Grant[];
  identities: {
    id: string;
    issuer: string;
    email?: string | null;
    created_at: string;
  }[];
  memberships: { data: Membership[]; has_more: boolean };
}

interface AuthConfig {
  methods: string[];
  oidc_providers: string[];
  captcha: { provider: string; site_key: string; action: string } | null;
}

export interface Challenge {
  id: string;
  method: string;
  expires_at: string;
  authorization_url?: string | null;
  /** WebAuthn options for a passkey ceremony (`publicKey` inside). */
  options?: Record<string, unknown> | null;
}

interface WorkspaceToken {
  token: string;
  expires_at: string;
  role: MembershipRole;
  scopes: string[];
}

/** An OAuth client asking for access: shown on the consent and device-approval pages. */
interface ConsentDetails {
  client_id: string;
  client_name: string;
  /** Where the browser returns after a code grant; absent for a device code. */
  redirect_host?: string;
  scopes: string[];
  resource: string;
  user_code?: string;
  expires_at: string;
}

export interface Member {
  id: string;
  user: { id: string; email: string; name?: string | null };
  role: MembershipRole;
  status: string;
  source: string;
  created_at: string;
  updated_at: string;
  version: number;
}

export interface Invitation {
  id: string;
  email: string;
  role: MembershipRole;
  status: "pending" | "accepted" | "revoked" | "expired";
  invited_by: string;
  expires_at: string;
  created_at: string;
}

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  mode: WorkspaceMode;
  scopes: string[];
  status: "active" | "revoked" | "expired";
  created_by: string;
  created_at: string;
  updated_at: string;
  expires_at?: string | null;
  last_used_at?: string | null;
  revoked_at?: string | null;
  /** Shown once, in the answer that creates the key. */
  secret?: string | null;
  version: number;
}

export interface AuditEntry {
  id: string;
  action: string;
  actor: { id: string; kind: string };
  target?: string | null;
  details: Record<string, unknown>;
  created_at: string;
}

/** Tokens are renewed this long before they expire, so no request carries a dying one. */
const TOKEN_MARGIN_MS = 30_000;

const parse = (text: string): unknown => {
  if (!text) {
    return undefined;
  }
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
};

/** One request to the API under `/api`; a failure is the SDK's own error type. */
const call = async <T>(
  method: string,
  path: string,
  init: {
    body?: unknown;
    csrf?: string;
    bearer?: string;
    idempotent?: boolean;
    signal?: AbortSignal;
    /** More headers, such as `If-Match` with a resource's `version`. */
    headers?: Record<string, string>;
  } = {}
): Promise<T> => {
  const headers = new Headers({ accept: "application/json" });
  if (init.body !== undefined) {
    headers.set("content-type", "application/json");
  }
  if (init.csrf) {
    headers.set("x-csrf-token", init.csrf);
  }
  if (init.bearer) {
    headers.set("authorization", `Bearer ${init.bearer}`);
  }
  for (const [name, value] of Object.entries(init.headers ?? {})) {
    headers.set(name, value);
  }
  if (init.idempotent) {
    headers.set("idempotency-key", crypto.randomUUID());
  }
  let response: Response;
  try {
    response = await fetch(`${BASE}${path}`, {
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
      credentials: "same-origin",
      headers,
      method,
      signal: init.signal,
    });
  } catch (error) {
    if (init.signal?.aborted) {
      throw error;
    }
    throw new APIConnectionError(
      "The request could not reach the Norbelys API.",
      { cause: error, timeout: false }
    );
  }
  const data = parse(await response.text());
  if (!response.ok) {
    throw new APIError(response.status, data, response.headers);
  }
  return data as T;
};

/** Whether an error means "nobody is signed in". */
export const isSignedOut = (error: unknown): boolean =>
  error instanceof APIError && error.status === 401;

// The anonymous sign-in calls carry any value in X-CSRF-Token: a custom header a cross-site form
// cannot send, which is the API's guard against login CSRF.
const SIGN_IN_HEADER = "sign-in";

/** Signing in: the methods the API offers, email codes and passkeys. */
export const auth = {
  config: (signal?: AbortSignal) =>
    call<AuthConfig>("GET", "/v1/auth/config", { signal }),

  /** Sends a 6-digit code (and a link) to `email`. Answers the same for every address. */
  startEmail: (email: string, captchaToken?: string) =>
    call<Challenge>("POST", "/v1/auth/challenges", {
      body: { captcha_token: captchaToken, email, method: "email_code" },
      csrf: SIGN_IN_HEADER,
    }),

  /** Finishes with the code, from the browser that asked for it. */
  finishCode: (challengeId: string, code: string) =>
    call<{ csrf_token: string }>("POST", "/v1/auth/sessions", {
      body: { challenge_id: challengeId, code },
      csrf: SIGN_IN_HEADER,
    }),

  /** Finishes with the mailed link's token, from any browser, for the address the page names. */
  finishLink: (token: string, email: string) =>
    call<{ csrf_token: string }>("POST", "/v1/auth/sessions", {
      body: { email, token },
      csrf: SIGN_IN_HEADER,
    }),

  startPasskey: () =>
    call<Challenge>("POST", "/v1/auth/challenges", {
      body: { method: "passkey" },
      csrf: SIGN_IN_HEADER,
    }),

  /**
   * Starts signing in with an identity provider the deployment offers (`google`, `microsoft`,
   * `github`); the challenge's `authorization_url` is where the browser goes next, and the API's
   * callback brings it back to `returnTo` (a dashboard path).
   */
  startProvider: (provider: string, returnTo?: string) =>
    call<Challenge>("POST", "/v1/auth/challenges", {
      body: { method: "oidc", provider, return_to: returnTo },
      csrf: SIGN_IN_HEADER,
    }),

  finishPasskey: (challengeId: string, credential: unknown) =>
    call<{ csrf_token: string }>("POST", "/v1/auth/sessions", {
      body: { challenge_id: challengeId, credential },
      csrf: SIGN_IN_HEADER,
    }),
};

/** The signed-in person, or `null` when the browser holds no live session. */
export const fetchMe = async (signal?: AbortSignal): Promise<Me | null> => {
  try {
    return await call<Me>("GET", "/v1/me", { signal });
  } catch (error) {
    if (isSignedOut(error)) {
      return null;
    }
    throw error;
  }
};

/**
 * What a consent or device-approval page asks about: the client, the scopes and the resource of a
 * sealed `/oauth/authorize` request (`request`) or of a device's user code (`user_code`). A read
 * with the session cookie.
 */
export const consentDetails = (query: {
  request?: string;
  user_code?: string;
}) => {
  const params = new URLSearchParams();
  if (query.request) {
    params.set("request", query.request);
  }
  if (query.user_code) {
    params.set("user_code", query.user_code);
  }
  return call<ConsentDetails>("GET", `/oauth/consent?${params}`);
};

/**
 * What a signed-in browser can do: its session's own operations, workspace tokens, and an SDK
 * client per workspace. One instance per session (its CSRF token belongs to it).
 */
export class Session {
  me: Me;
  readonly #tokens = new Map<string, WorkspaceToken>();
  readonly #pending = new Map<string, Promise<WorkspaceToken>>();
  readonly #clients = new Map<string, Norbelys>();

  constructor(me: Me) {
    this.me = me;
  }

  get csrf(): string {
    return this.me.csrf_token;
  }

  /** Active memberships, the workspaces this person can open. */
  get memberships(): Membership[] {
    return this.me.memberships.data.filter((m) => m.status === "active");
  }

  membership(slugOrId: string): Membership | undefined {
    return this.memberships.find(
      (m) => m.workspace.slug === slugOrId || m.workspace.id === slugOrId
    );
  }

  /** A workspace token, minted from the session when the cached one is missing or dying. */
  async token(workspaceId: string): Promise<string> {
    const cached = this.#tokens.get(workspaceId);
    if (
      cached &&
      Date.parse(cached.expires_at) - Date.now() > TOKEN_MARGIN_MS
    ) {
      return cached.token;
    }
    let pending = this.#pending.get(workspaceId);
    if (!pending) {
      const mint = async () => {
        try {
          return await call<WorkspaceToken>("POST", "/v1/auth/tokens", {
            body: { workspace_id: workspaceId },
            csrf: this.csrf,
          });
        } finally {
          this.#pending.delete(workspaceId);
        }
      };
      pending = mint();
      this.#pending.set(workspaceId, pending);
    }
    const minted = await pending;
    this.#tokens.set(workspaceId, minted);
    return minted.token;
  }

  /** The public SDK, acting in one workspace with this session's tokens. */
  client(workspaceId: string): Norbelys {
    let client = this.#clients.get(workspaceId);
    if (!client) {
      client = new Norbelys({
        baseUrl: BASE,
        token: () => this.token(workspaceId),
      });
      this.#clients.set(workspaceId, client);
    }
    return client;
  }

  /** A dashboard-surface operation of a workspace (members, keys, invitations, audit log). */
  async workspace<T>(
    workspaceId: string,
    method: string,
    path: string,
    init: {
      body?: unknown;
      idempotent?: boolean;
      signal?: AbortSignal;
      headers?: Record<string, string>;
    } = {}
  ): Promise<T> {
    const bearer = await this.token(workspaceId);
    return await call<T>(
      method,
      `/v1/workspaces/${encodeURIComponent(workspaceId)}${path}`,
      { ...init, bearer }
    );
  }

  /** Creates a workspace the person owns; it is live (the dashboard creates no test workspaces). */
  createWorkspace(body: { name: string; slug?: string; timezone?: string }) {
    return call<WorkspaceSummary & { timezone: string }>(
      "POST",
      "/v1/workspaces",
      { body, csrf: this.csrf, idempotent: true }
    );
  }

  updateMe(body: { name?: string | null; locale?: string }) {
    return call<Me>("PATCH", "/v1/me", { body, csrf: this.csrf });
  }

  /** Starts registering a passkey for this person (finished by `addPasskey`). */
  startPasskeyRegistration() {
    return call<Challenge>("POST", "/v1/auth/challenges", {
      body: { method: "passkey_registration" },
      csrf: this.csrf,
    });
  }

  addPasskey(challengeId: string, credential: unknown, name: string) {
    return call<Passkey>("POST", "/v1/me/passkeys", {
      body: { challenge_id: challengeId, credential, name },
      csrf: this.csrf,
      idempotent: true,
    });
  }

  revokeSession(id: string) {
    return call<unknown>("DELETE", `/v1/me/sessions/${id}`, {
      csrf: this.csrf,
    });
  }

  deletePasskey(id: string) {
    return call<unknown>("DELETE", `/v1/me/passkeys/${id}`, {
      csrf: this.csrf,
    });
  }

  /** Approves (in one workspace) or denies a consent request or a device code. */
  decideConsent(body: {
    request?: string;
    user_code?: string;
    workspace_id?: string;
    approve: boolean;
  }) {
    return call<{ approved: boolean; redirect_to?: string; grant_id?: string }>(
      "POST",
      "/oauth/consent",
      { body, csrf: this.csrf }
    );
  }

  /** Registers a fresh set of owner recovery codes, shown once; the previous set stops working. */
  createRecoveryCodes() {
    return call<{ codes: string[]; created_at: string }>(
      "POST",
      "/v1/me/recovery_codes",
      { csrf: this.csrf }
    );
  }

  unlinkIdentity(id: string) {
    return call<unknown>("DELETE", `/v1/me/identities/${id}`, {
      csrf: this.csrf,
    });
  }

  revokeGrant(id: string) {
    return call<unknown>("DELETE", `/v1/me/grants/${id}`, { csrf: this.csrf });
  }

  acceptInvitation(token: string) {
    return call<Membership>("POST", "/v1/me/memberships", {
      body: { invitation_token: token },
      csrf: this.csrf,
    });
  }

  /** Ends this browser's session; the cookie stops working at once. */
  async signOut(): Promise<void> {
    await this.revokeSession(this.me.session_id);
    this.#tokens.clear();
    this.#clients.clear();
  }
}
