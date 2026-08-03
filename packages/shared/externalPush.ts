export type ExternalPushState =
  | "pending"
  | "sending"
  | "uncertain"
  | "failed"
  | "cancelled"
  | "succeeded";

export type ExternalPushRetryGuidance =
  | "poll"
  | "complete"
  | "new_request_after_correction"
  | "contact_admin_for_reconciliation"
  | "new_request"
  | "contact_support";

export type ExternalPushAccepted = {
  attempt_id: string;
  status: ExternalPushState;
  status_url: string;
};

export type ExternalPushStatus = {
  attempt_id: string;
  integration: "crm" | "desk";
  state: ExternalPushState;
  attempt_count: number;
  remote_id?: string | null;
  remote_url?: string | null;
  error_code?: string | null;
  retry_guidance: ExternalPushRetryGuidance;
  created_at: string;
  updated_at: string;
};

export type ExternalPushSubmission<T> =
  | { kind: "legacy"; value: T }
  | { kind: "accepted"; value: ExternalPushAccepted };

export type ExternalPushResult<T> =
  | { kind: "legacy"; value: T }
  | { kind: "external"; value: ExternalPushStatus };

export class ExternalPushSettlementError extends Error {
  status: number;
  retryGuidance?: ExternalPushRetryGuidance;
  code:
    | "external_push_failed"
    | "external_push_pending"
    | "external_push_protocol"
    | "external_push_uncertain";

  constructor(
    code: ExternalPushSettlementError["code"],
    message: string,
    status: number,
    retryGuidance?: ExternalPushRetryGuidance,
  ) {
    super(message);
    this.name = "ExternalPushSettlementError";
    this.code = code;
    this.status = status;
    this.retryGuidance = retryGuidance;
  }
}

/** Internal control-flow signal used when a route/modal is torn down while
 * polling. It must not be rendered as a failed remote push. */
export class ExternalPushPollingCancelled extends Error {
  constructor() {
    super("External push polling was cancelled.");
    this.name = "ExternalPushPollingCancelled";
  }
}

const STATES = new Set<ExternalPushState>([
  "pending",
  "sending",
  "uncertain",
  "failed",
  "cancelled",
  "succeeded",
]);
const GUIDANCE = new Set<ExternalPushRetryGuidance>([
  "poll",
  "complete",
  "new_request_after_correction",
  "contact_admin_for_reconciliation",
  "new_request",
  "contact_support",
]);
const GUIDANCE_BY_STATE: Record<
  ExternalPushState,
  ExternalPushRetryGuidance
> = {
  pending: "poll",
  sending: "poll",
  uncertain: "contact_admin_for_reconciliation",
  failed: "new_request_after_correction",
  cancelled: "new_request",
  succeeded: "complete",
};
const UUID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

function protocolError(message: string): ExternalPushSettlementError {
  return new ExternalPushSettlementError(
    "external_push_protocol",
    message,
    502,
  );
}

/** Treat integration deep links as untrusted input. Only absolute HTTPS URLs
 * without embedded credentials are suitable for browser/native openers. */
export function externalHttpsUrl(raw: unknown): string | null {
  if (typeof raw !== "string" || raw.length === 0 || raw !== raw.trim()) {
    return null;
  }
  try {
    const parsed = new URL(raw);
    if (
      parsed.protocol !== "https:" ||
      !parsed.hostname ||
      parsed.username ||
      parsed.password
    ) {
      return null;
    }
    return parsed.href;
  } catch {
    return null;
  }
}

/** Generate a cryptographically secure UUID for one semantic push action. */
export function createExternalPushIdempotencyKey(): string {
  const cryptoApi = globalThis.crypto;
  if (cryptoApi?.randomUUID) return cryptoApi.randomUUID();
  if (!cryptoApi?.getRandomValues) {
    throw new Error("Secure idempotency keys are unavailable.");
  }

  const bytes = cryptoApi.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return (
    `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}` +
    `-${hex.slice(16, 20)}-${hex.slice(20)}`
  );
}

/** Parse the server's 202 envelope before using its path or attempt id. */
export function parseExternalPushAccepted(
  input: unknown,
): ExternalPushAccepted {
  if (!input || typeof input !== "object") {
    throw protocolError("The server returned an invalid push response.");
  }
  const value = input as Record<string, unknown>;
  if (
    typeof value.attempt_id !== "string" ||
    !UUID.test(value.attempt_id) ||
    typeof value.status_url !== "string" ||
    typeof value.status !== "string" ||
    !STATES.has(value.status as ExternalPushState)
  ) {
    throw protocolError("The server returned an invalid push response.");
  }
  return {
    attempt_id: value.attempt_id,
    status: value.status as ExternalPushState,
    status_url: value.status_url,
  };
}

/** Tauri returns either the legacy 200 body or the 202 body as JSON without
 * exposing the HTTP status. Recognize any attempted 202 envelope and validate
 * it strictly; do not silently reinterpret a malformed envelope as legacy. */
export function externalPushSubmissionFromWire<T>(
  input: unknown,
): ExternalPushSubmission<T> {
  if (
    input &&
    typeof input === "object" &&
    ("attempt_id" in input || "status_url" in input || "status" in input)
  ) {
    return { kind: "accepted", value: parseExternalPushAccepted(input) };
  }
  return { kind: "legacy", value: input as T };
}

/** Only poll the exact call/attempt endpoint described by the public contract.
 * This prevents a compromised/malformed response from turning the authenticated
 * client into an arbitrary API-path requester. */
export function assertExternalPushStatusPath(
  callId: string,
  accepted: ExternalPushAccepted,
): string {
  const expected =
    `/v1/calls/${encodeURIComponent(callId)}` +
    `/external-pushes/${encodeURIComponent(accepted.attempt_id)}`;
  if (accepted.status_url !== expected) {
    throw protocolError("The server returned an invalid push status URL.");
  }
  return expected;
}

export function parseExternalPushStatus(
  input: unknown,
  accepted: ExternalPushAccepted,
  integration: ExternalPushStatus["integration"],
): ExternalPushStatus {
  if (!input || typeof input !== "object") {
    throw protocolError("The server returned an invalid push status.");
  }
  const value = input as Record<string, unknown>;
  if (
    value.attempt_id !== accepted.attempt_id ||
    value.integration !== integration ||
    typeof value.state !== "string" ||
    !STATES.has(value.state as ExternalPushState) ||
    typeof value.attempt_count !== "number" ||
    !Number.isInteger(value.attempt_count) ||
    value.attempt_count < 0 ||
    typeof value.retry_guidance !== "string" ||
    !GUIDANCE.has(value.retry_guidance as ExternalPushRetryGuidance) ||
    GUIDANCE_BY_STATE[value.state as ExternalPushState] !==
      value.retry_guidance ||
    typeof value.created_at !== "string" ||
    Number.isNaN(Date.parse(value.created_at)) ||
    typeof value.updated_at !== "string" ||
    Number.isNaN(Date.parse(value.updated_at)) ||
    (value.remote_id != null && typeof value.remote_id !== "string") ||
    (value.remote_url != null &&
      externalHttpsUrl(value.remote_url) === null) ||
    (value.error_code != null && typeof value.error_code !== "string")
  ) {
    throw protocolError("The server returned an invalid push status.");
  }
  return value as ExternalPushStatus;
}

function settlementError(
  status: ExternalPushStatus,
): ExternalPushSettlementError {
  if (status.state === "uncertain") {
    return new ExternalPushSettlementError(
      "external_push_uncertain",
      "The server could not confirm whether the push completed. Do not start another push yet.",
      409,
      status.retry_guidance,
    );
  }
  return new ExternalPushSettlementError(
    "external_push_failed",
    status.state === "cancelled"
      ? "The push was cancelled."
      : "The push failed. Correct the integration or record details before trying again.",
    502,
    status.retry_guidance,
  );
}

function throwIfCancelled(isCancelled?: () => boolean): void {
  if (isCancelled?.()) throw new ExternalPushPollingCancelled();
}

/** Submit exactly once, then settle a 202 exclusively through GET polling.
 * Callers retain the same Idempotency-Key if the whole logical action is
 * retried; this function never repeats the POST itself. */
export async function runExternalPush<T>(options: {
  submit: () => Promise<ExternalPushSubmission<T>>;
  loadStatus: (
    statusUrl: string,
    accepted: ExternalPushAccepted,
  ) => Promise<ExternalPushStatus>;
  wait: (poll: number) => Promise<void>;
  /** Resume an already-accepted action without issuing POST again. */
  accepted?: ExternalPushAccepted;
  /** Persist the accepted envelope before the first status request. */
  onAccepted?: (accepted: ExternalPushAccepted) => void;
  isCancelled?: () => boolean;
  maxPolls?: number;
}): Promise<ExternalPushResult<T>> {
  throwIfCancelled(options.isCancelled);
  const submission: ExternalPushSubmission<T> = options.accepted
    ? {
        kind: "accepted",
        value: parseExternalPushAccepted(options.accepted),
      }
    : await options.submit();
  throwIfCancelled(options.isCancelled);
  if (submission.kind === "legacy") return submission;

  const accepted = submission.value;
  options.onAccepted?.(accepted);
  const maxPolls = Math.max(1, options.maxPolls ?? 80);
  for (let poll = 0; poll < maxPolls; poll += 1) {
    throwIfCancelled(options.isCancelled);
    let status: ExternalPushStatus;
    try {
      status = await options.loadStatus(accepted.status_url, accepted);
    } catch (error) {
      throwIfCancelled(options.isCancelled);
      if (
        error instanceof ExternalPushSettlementError ||
        error instanceof ExternalPushPollingCancelled
      ) {
        throw error;
      }
      // Once admission is known, a status-read failure cannot prove delivery
      // failed. Keep the attempt open and permit only GET-based resumption.
      throw new ExternalPushSettlementError(
        "external_push_pending",
        "The push status could not be confirmed. Retry will continue the same request.",
        202,
        "poll",
      );
    }
    throwIfCancelled(options.isCancelled);
    if (status.state === "succeeded") {
      return { kind: "external", value: status };
    }
    if (
      status.state === "failed" ||
      status.state === "cancelled" ||
      status.state === "uncertain"
    ) {
      throw settlementError(status);
    }
    if (poll + 1 < maxPolls) {
      await options.wait(poll);
      throwIfCancelled(options.isCancelled);
    }
  }

  throw new ExternalPushSettlementError(
    "external_push_pending",
    "The push is still processing. Check the call before starting another push.",
    202,
    "poll",
  );
}
