import { invoke } from "@tauri-apps/api/core";
import {
  ExternalPushPollingCancelled,
  ExternalPushSettlementError,
  assertExternalPushStatusPath,
  externalPushSubmissionFromWire,
  parseExternalPushStatus,
  runExternalPush,
  type ExternalPushAccepted,
  type ExternalPushResult,
  type ExternalPushStatus,
} from "../../packages/shared/externalPush";
import { isPortalError } from "$lib/portalError";

export {
  ExternalPushPollingCancelled,
  ExternalPushSettlementError,
  createExternalPushIdempotencyKey,
  externalHttpsUrl,
} from "../../packages/shared/externalPush";
export type {
  ExternalPushAccepted,
} from "../../packages/shared/externalPush";

function waitWithAbort(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) {
    return Promise.reject(new ExternalPushPollingCancelled());
  }
  return new Promise((resolve, reject) => {
    const timer = window.setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(new ExternalPushPollingCancelled());
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/** Desktop bridge for the shared durable-push protocol. The native POST command
 * returns either a legacy 200 body or a 202 envelope. Once accepted, all
 * settlement traffic is GET-only through the fixed native status command. */
export async function runAgentExternalPush<T>(options: {
  callId: string;
  integration: ExternalPushStatus["integration"];
  submit: () => Promise<unknown>;
  accepted?: ExternalPushAccepted;
  onAccepted?: (accepted: ExternalPushAccepted) => void;
  signal?: AbortSignal;
}): Promise<ExternalPushResult<T>> {
  try {
    return await runExternalPush<T>({
      submit: async () =>
        externalPushSubmissionFromWire<T>(await options.submit()),
      accepted: options.accepted,
      onAccepted: options.onAccepted,
      loadStatus: async (reportedPath, accepted) => {
        assertExternalPushStatusPath(options.callId, {
          ...accepted,
          status_url: reportedPath,
        });
        const raw = await invoke<unknown>("external_push_status", {
          callId: options.callId,
          attemptId: accepted.attempt_id,
        });
        return parseExternalPushStatus(
          raw,
          accepted,
          options.integration,
        );
      },
      // Start quickly, then cap the backoff so a settlement remains visible
      // without hammering the backend. Forty polls span roughly 75 seconds.
      wait: (poll) =>
        waitWithAbort(
          Math.min(2_000, 300 * 2 ** Math.min(poll, 4)),
          options.signal,
        ),
      isCancelled: () => options.signal?.aborted === true,
      maxPolls: 40,
    });
  } catch (error) {
    if (options.signal?.aborted) {
      throw new ExternalPushPollingCancelled();
    }
    if (
      error instanceof ExternalPushSettlementError ||
      error instanceof ExternalPushPollingCancelled
    ) {
      throw error;
    }
    if (
      isPortalError(error) &&
      error.kind !== "network" &&
      error.kind !== "other"
    ) {
      throw error;
    }
    // A plain IPC rejection cannot prove whether POST bytes reached the
    // backend. The owning UI keeps this action's UUID, making Retry safe.
    throw new ExternalPushSettlementError(
      "external_push_pending",
      "The server response could not be confirmed. Retry will reuse the same request.",
      202,
      "poll",
    );
  }
}
