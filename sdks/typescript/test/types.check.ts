/** Type-level contract checks: compiled by typecheck, never executed. */
import type {
  APIError,
  CampaignObject,
  CampaignStatus,
  ConnectionObject,
  ConnectionStatus,
  ConsentAnswer,
  Enrolled,
  EnrollmentObject,
  EventObject,
  EventType,
  FieldType,
  JobObject,
  JobState,
  MessageObject,
  MessageState,
  Norbelys,
  ProblemCode,
  Provider,
  SmtpSecurity,
  StepResults,
  ThreadStatus,
  VariantObject,
  WebhookEvent,
} from "../src/index";
import { verifyWebhook } from "../src/index";

type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false;
type Expect<T extends true> = T;
type Result<F extends (...args: never[]) => unknown> = Awaited<ReturnType<F>>;

/** Enums are the published lowercase values: requests refuse others, responses name them. */
export type EnumChecks = [
  Expect<
    Equal<
      CampaignStatus,
      "draft" | "materialising" | "active" | "paused" | "completed" | "archived"
    >
  >,
  Expect<Equal<ThreadStatus, "open" | "snoozed" | "archived">>,
  Expect<Equal<SmtpSecurity, "tls" | "starttls" | "plain">>,
  Expect<
    Equal<
      Provider,
      | "smtp"
      | "google"
      | "microsoft"
      | "ses"
      | "sendgrid"
      | "mailgun"
      | "norbelys"
    >
  >,
  Expect<Equal<FieldType, "text" | "number" | "boolean" | "date" | "enum">>,
  Expect<
    Equal<
      MessageState,
      | "queued"
      | "claimed"
      | "in_flight"
      | "sent"
      | "failed"
      | "cancelled"
      | "uncertain"
      | "suppressed"
    >
  >,
  Expect<
    Equal<
      JobState,
      | "available"
      | "running"
      | "completed"
      | "failed"
      | "cancelled"
      | "needs_review"
    >
  >,
];

/** Response fields that hold a closed vocabulary are typed with it, not as plain strings. */
export type FieldChecks = [
  Expect<Equal<MessageObject["state"], MessageState>>,
  Expect<Equal<ConnectionObject["status"], ConnectionStatus>>,
  Expect<Equal<ConnectionObject["provider"], Provider>>,
  Expect<Equal<EventObject["type"], EventType>>,
  Expect<Equal<JobObject["state"], JobState>>,
  Expect<Equal<WebhookEvent["type"], EventType>>,
  Expect<Equal<APIError["code"], ProblemCode | undefined>>,
  // A variant's body is left out of a list, so it is optional.
  Expect<Equal<VariantObject["html"], string | undefined>>,
];

/** A method answers every success its operation declares, and nothing it does not. */
export type ResultChecks = [
  Expect<Equal<Result<Norbelys["groups"]["delete"]>, void>>,
  Expect<Equal<Result<Norbelys["enrollments"]["retrieve"]>, EnrollmentObject>>,
  Expect<Equal<Result<Norbelys["messages"]["retrieve"]>, MessageObject>>,
  Expect<
    Equal<Result<Norbelys["campaigns"]["delete"]>, CampaignObject | undefined>
  >,
  Expect<
    Equal<
      Result<Norbelys["connections"]["create"]>,
      ConnectionObject | ConsentAnswer
    >
  >,
  Expect<
    Equal<Result<Norbelys["enrollments"]["create"]>, Enrolled | JobObject>
  >,
];

/** Typed calls preserve media, filters, body shapes and conditional request options. */
export const requests = async (client: Norbelys): Promise<void> => {
  await client.connections.create({
    provider: "google",
    return_to: "/connections",
  });
  // @ts-expect-error -- only published providers are accepted
  await client.connections.create({ provider: "Google" });

  // Each form of `POST /messages` answers what it creates.
  const direct: MessageObject = await client.messages.create({
    from: "sender@example.com",
    to: ["ada@example.com"],
    subject: "Hello",
    html: "<p>Hello</p>",
  });
  const reply: MessageObject = await client.messages.create({
    thread_id: "thr_1",
    html: "<p>Thanks</p>",
  });
  const preview: MessageObject = await client.messages.create({
    step_id: "stp_1",
    person_id: "per_1",
    to: "max@example.com",
  });
  const many: StepResults = await client.messages.create({
    step_id: "stp_1",
    person_ids: ["per_1", "per_2"],
  });
  // @ts-expect-error -- several people answer a result each, not one message
  const notOne: MessageObject = await client.messages.create({
    step_id: "stp_1",
    person_ids: ["per_1"],
  });
  void [direct, reply, preview, many, notOne];
  // @ts-expect-error -- a direct message requires a subject
  await client.messages.create({
    from: "sender@example.com",
    to: ["ada@example.com"],
    html: "<p>Hello</p>",
  });
  // @ts-expect-error -- a body is one form: a step's content takes no subject
  await client.messages.create({
    step_id: "stp_1",
    person_id: "per_1",
    subject: "Hi",
  });

  await client.imports.create("email\nada@example.com\n", {
    group_id: "grp_1",
  });
  await client.images.create({ data: new Blob(), contentType: "image/png" });
  await client.images.create({
    data: new Blob(),
    // @ts-expect-error -- executable image formats are refused
    contentType: "image/svg+xml",
  });

  // An update takes the version it was prepared from; other operations have no precondition.
  await client.groups.update("grp_1", { description: null }, { ifMatch: 1 });
  await client.groups.update("grp_1", { name: "VIP" }, { ifMatch: '"1"' });
  // @ts-expect-error -- only updates take `ifMatch`
  await client.groups.retrieve("grp_1", { ifMatch: 1 });

  // Methods are camelCase; the operation ids keep their snake_case.
  await client.messages.releaseHolds("msg_1", { evidence: "Checked" });
  await client.webhookEndpoints.rotateSecret("whe_1");
  // @ts-expect-error -- the operation id is not the method name
  await client.webhookEndpoints.rotate_secret("whe_1");

  await client.campaigns.list({ status: "active", limit: 20 });
  // @ts-expect-error -- enum values are case-sensitive
  await client.campaigns.list({ status: "Active" });
  await client.messages.list({ state: "sent", order: "asc" });
  // @ts-expect-error -- a filter takes the published states only
  await client.messages.list({ state: "delivered" });
  // @ts-expect-error -- page numbers do not exist in the cursor contract
  await client.people.list({ page: 2 });
  const page = await client.people.list({
    include: "total_count",
    group_id: "grp_1",
  });
  // @ts-expect-error -- `include` names what the page adds
  await client.people.list({ include: "count" });
  const total: number | null | undefined = page.meta.total_count;
  void total;

  const event = await verifyWebhook("{}", new Headers(), "whsec_c2VjcmV0");
  const type: EventType = event.type;
  void type;
};
