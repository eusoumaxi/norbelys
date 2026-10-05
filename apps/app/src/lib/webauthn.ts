/**
 * Passkeys in the browser: the API's challenge options (WebAuthn's JSON form, binary members as
 * base64url, under `publicKey`) to a credential request, and the credential back to JSON. The
 * browser's own `parse…FromJSON` and `toJSON()` are used where they exist; the fallback converts
 * the same members by hand.
 */

const fromBase64Url = (value: string): ArrayBuffer => {
  const base64 = value.replaceAll("-", "+").replaceAll("_", "/");
  const padded = base64 + "=".repeat((4 - (base64.length % 4)) % 4);
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.codePointAt(index) ?? 0;
  }
  return bytes.buffer;
};

const toBase64Url = (buffer: ArrayBuffer | null | undefined): string | null => {
  if (!buffer) {
    return null;
  }
  let binary = "";
  for (const byte of new Uint8Array(buffer)) {
    binary += String.fromCodePoint(byte);
  }
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
};

type Json = Record<string, unknown>;

interface CredentialStatics {
  parseRequestOptionsFromJSON?: (
    options: Json
  ) => PublicKeyCredentialRequestOptions;
  parseCreationOptionsFromJSON?: (
    options: Json
  ) => PublicKeyCredentialCreationOptions;
}

const statics = (): CredentialStatics =>
  (globalThis.PublicKeyCredential ?? {}) as unknown as CredentialStatics;

const withIds = (list: unknown) =>
  Array.isArray(list)
    ? list.map((item: Json) => ({
        ...item,
        id: fromBase64Url(String(item.id)),
      }))
    : undefined;

const requestOptions = (publicKey: Json): PublicKeyCredentialRequestOptions =>
  statics().parseRequestOptionsFromJSON?.(publicKey) ??
  ({
    ...publicKey,
    allowCredentials: withIds(publicKey.allowCredentials),
    challenge: fromBase64Url(String(publicKey.challenge)),
  } as PublicKeyCredentialRequestOptions);

const creationOptions = (publicKey: Json): PublicKeyCredentialCreationOptions =>
  statics().parseCreationOptionsFromJSON?.(publicKey) ??
  ({
    ...publicKey,
    challenge: fromBase64Url(String(publicKey.challenge)),
    excludeCredentials: withIds(publicKey.excludeCredentials),
    user: {
      ...(publicKey.user as Json),
      id: fromBase64Url(String((publicKey.user as Json).id)),
    },
  } as PublicKeyCredentialCreationOptions);

const toJson = (credential: PublicKeyCredential): Json => {
  const native = (credential as unknown as { toJSON?: () => Json }).toJSON;
  if (native) {
    return native.call(credential);
  }
  const response = credential.response as AuthenticatorAssertionResponse &
    AuthenticatorAttestationResponse;
  return {
    clientExtensionResults: credential.getClientExtensionResults(),
    id: credential.id,
    rawId: toBase64Url(credential.rawId),
    response: {
      attestationObject: toBase64Url(response.attestationObject),
      authenticatorData: toBase64Url(response.authenticatorData),
      clientDataJSON: toBase64Url(response.clientDataJSON),
      signature: toBase64Url(response.signature),
      userHandle: toBase64Url(response.userHandle),
    },
    type: credential.type,
  };
};

/** Whether this browser can use passkeys at all. */
export const passkeysSupported = (): boolean =>
  typeof window !== "undefined" && "PublicKeyCredential" in window;

const publicKeyOf = (options: Json | null | undefined): Json => {
  const publicKey = options?.publicKey;
  if (!publicKey || typeof publicKey !== "object") {
    throw new Error("The API did not send passkey options.");
  }
  return publicKey as Json;
};

/** Asks the browser for an assertion with the challenge's options; `null` when the person cancels. */
export const getPasskey = async (options: Json | null | undefined) => {
  const credential = (await navigator.credentials.get({
    publicKey: requestOptions(publicKeyOf(options)),
  })) as PublicKeyCredential | null;
  return credential ? toJson(credential) : null;
};

/** Asks the browser to create a passkey with the challenge's options; `null` when cancelled. */
export const createPasskey = async (options: Json | null | undefined) => {
  const credential = (await navigator.credentials.create({
    publicKey: creationOptions(publicKeyOf(options)),
  })) as PublicKeyCredential | null;
  return credential ? toJson(credential) : null;
};
