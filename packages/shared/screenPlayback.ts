import type { ScreenPlaybackUrl, ScreenRecording } from "./types";

export type ScreenPlaybackCandidateState =
  | "ready"
  | "recording_not_ready"
  | "generation_mismatch"
  | "expired"
  | "invalid";

/** Validate the two-response playback contract before a credential is ever
 * attached to a media element. This stays pure so both surfaces share the
 * same generation and expiry rules. */
export function classifyScreenPlaybackCandidate(
  recording: ScreenRecording | null | undefined,
  candidate: ScreenPlaybackUrl,
  nowMs = Date.now(),
): ScreenPlaybackCandidateState {
  if (recording?.status !== "ready") return "recording_not_ready";
  if (candidate.generation_id !== recording.generation_id) {
    return "generation_mismatch";
  }
  if (!candidate.playback_url.trim()) return "invalid";

  const expiresAtMs = Date.parse(candidate.playback_url_expires_at);
  if (!Number.isFinite(expiresAtMs)) return "invalid";
  if (expiresAtMs <= nowMs) return "expired";
  return "ready";
}

/** Delay before proactively refreshing an expanded player's credential.
 * Refresh at ten percent of the TTL (capped at fifteen seconds) before
 * expiry, leaving enough margin for clock skew without churning long TTLs. */
export function screenPlaybackRefreshDelayMs(
  expiresAt: string,
  nowMs = Date.now(),
): number | null {
  const expiresAtMs = Date.parse(expiresAt);
  if (!Number.isFinite(expiresAtMs)) return null;
  const ttlMs = expiresAtMs - nowMs;
  if (ttlMs <= 0) return 0;
  const marginMs = Math.min(15_000, Math.max(1_000, ttlMs / 10));
  return Math.max(0, ttlMs - marginMs);
}
