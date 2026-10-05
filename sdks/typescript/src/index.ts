import type { ClientOptions } from "./core";
import { Core } from "./core";
import { NorbelysResources } from "./generated/resources";

/**
 * The Norbelys API client.
 *
 * ```ts
 * const norbelys = new Norbelys(); // reads NORBELYS_API_KEY
 * const campaign = await norbelys.campaigns.retrieve("cmp_...");
 * ```
 */
export class Norbelys extends NorbelysResources {
  constructor(options: ClientOptions = {}) {
    super(new Core(options));
  }
}

export default Norbelys;

export type {
  ClientOptions,
  OneOf,
  RequestOptions,
  UpdateOptions,
} from "./core";
export {
  APIConnectionError,
  APIError,
  NorbelysError,
  PollTimeoutError,
  WebhookVerificationError,
} from "./errors";
export { type Page, PagePromise } from "./pagination";
export { poll, type PollOptions } from "./poll";
export {
  verifyWebhook,
  type VerifyWebhookOptions,
  type WebhookEvent,
  type WebhookHeaders,
} from "./webhooks";
export type * from "./generated/schema";
