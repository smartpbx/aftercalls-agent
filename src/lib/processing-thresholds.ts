// Processing-delay UX (#286) — derived "still working" state.
//
// The backend exposes the raw status string (`uploading`, `transcribing`,
// `transcribed`, `summarizing`, `complete`, `failed`). For "is this call
// taking longer than usual?" we don't add a backend column — we just
// compare the call's age (now - recorded_at) against a per-stage
// threshold. After the threshold the UI swaps the neutral "Processing"
// pill for a "Still working" treatment + a one-line reassurance.
//
// Threshold defaults are conservative: typical transcribe completes
// well under 5 minutes, typical summarize under 10. Operators can
// raise/lower via VITE env vars at build time without touching code.
//
// MIRROR PAIR: this file is byte-identical at
// `agent/src/lib/processing-thresholds.ts`. Any edit must apply to
// both files in the same commit.

/** Minutes after which a call still in transcribe-stage is "delayed". */
const TRANSCRIBE_THRESHOLD_MIN = readMinutesEnv(
  "VITE_PROCESSING_DELAY_TRANSCRIBE_MIN",
  5,
);

/** Minutes after which a call still in summarize-stage is "delayed". */
const SUMMARIZE_THRESHOLD_MIN = readMinutesEnv(
  "VITE_PROCESSING_DELAY_SUMMARIZE_MIN",
  10,
);

function readMinutesEnv(key: string, fallback: number): number {
  try {
    const raw = (import.meta as any).env?.[key];
    if (raw === undefined || raw === null || raw === "") return fallback;
    const n = Number(raw);
    if (!Number.isFinite(n) || n <= 0) return fallback;
    return n;
  } catch {
    return fallback;
  }
}

/** Statuses that mean the pipeline is still running. The backend
 *  flips to `complete` or `failed` (TERMINAL_STATES) at the end. */
const PROCESSING_STATUSES = new Set([
  "uploading",
  "transcribing",
  "transcribed",
  "summarizing",
]);

/** Coarse stage bucket — drives which threshold applies. We bucket
 *  `uploading` + `transcribing` under "transcribe" because both are
 *  pre-summarize work, and `transcribed` + `summarizing` under
 *  "summarize" because the call is past transcription. */
type Stage = "transcribe" | "summarize" | "other";

function stageFor(status: string): Stage {
  if (status === "uploading" || status === "transcribing") return "transcribe";
  if (status === "transcribed" || status === "summarizing") return "summarize";
  return "other";
}

/** True when the call is mid-pipeline (not complete/failed). */
export function isProcessing(status: string): boolean {
  return PROCESSING_STATUSES.has(status);
}

/** Minutes elapsed since the call was recorded. Defensive against a
 *  malformed timestamp — returns 0 rather than NaN so callers don't
 *  accidentally treat a parse failure as "delayed forever". */
export function ageMinutes(recordedAt: string, nowMs: number = Date.now()): number {
  const t = Date.parse(recordedAt);
  if (!Number.isFinite(t)) return 0;
  const diff = (nowMs - t) / 60_000;
  return diff > 0 ? diff : 0;
}

/** Threshold (minutes) for the call's current stage. Returns
 *  `Infinity` for stages we don't gate, so the comparison never
 *  fires and the pill stays neutral. */
export function thresholdMinutes(status: string): number {
  switch (stageFor(status)) {
    case "transcribe":
      return TRANSCRIBE_THRESHOLD_MIN;
    case "summarize":
      return SUMMARIZE_THRESHOLD_MIN;
    default:
      return Infinity;
  }
}

/** True when the call is processing AND has been processing longer
 *  than the threshold for its current stage. Pure: same inputs →
 *  same output, so it's safe to call from a Svelte `$derived`. */
export function isDelayed(
  status: string,
  recordedAt: string,
  nowMs: number = Date.now(),
): boolean {
  if (!isProcessing(status)) return false;
  return ageMinutes(recordedAt, nowMs) >= thresholdMinutes(status);
}

/** Pill copy for the calls list + detail header. Vendor-opaque by
 *  design: never mention the cause of the slowness. */
export const PILL_PROCESSING = "Processing";
export const PILL_STILL_WORKING = "Still working";

/** One-line reassurance shown under the call title on the detail
 *  page when the call is past threshold. Commits to a notification
 *  rather than the vague "you'll see it when ready" framing. */
export const REASSURANCE_LINE =
  "Still working — we'll notify you when this is ready.";

/** Toast message fired when a previously-delayed call finishes.
 *  `<title>` substitution happens at the call site. */
export function readyToastMessage(title: string): string {
  const t = title?.trim();
  return t ? `Your call '${t}' is ready.` : "Your call is ready.";
}

/** Tray-notification body for the agent. Same vendor-opaque tone as
 *  the toast. */
export function readyTrayBody(title: string): string {
  const t = title?.trim();
  return t ? `'${t}' is ready to view.` : "Your call is ready to view.";
}

/** Exposed for tests + diagnostics. Not used by the UI directly. */
export const _internal = {
  TRANSCRIBE_THRESHOLD_MIN,
  SUMMARIZE_THRESHOLD_MIN,
  stageFor,
};
