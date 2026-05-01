// Shared helper packaged at `@aftercalls/shared/time`. It lives under
// `agent/packages/shared` so the public agent mirror carries it, while
// the monorepo `packages/shared` entry points at the same package.
// Added in v0.4.0 Phase 4 (#105) for the /actions row-context line
// (`{call-title} · {relative-time}`).
//
// No deps — plain Date arithmetic. Locale is hard-coded to en-US
// because the app is English-only today; when we add i18n this
// helper will grow an `Intl.RelativeTimeFormat` path and a locale
// prop, but shipping that machinery now would be speculation.
//
// Buckets (mirrors ui-phase-4 §I):
//   < 60s       → "just now"
//   < 60m       → "N min ago"
//   < 24h       → "N hr ago"
//   < 7d        → "N days ago"
//   < 1y        → absolute short date ("Nov 15")
//   >= 1y       → absolute date with year ("Nov 15 2025")
//
// Negative deltas (input is in the future) fall through to the
// absolute-date branches so a clock-skew row doesn't read as
// "-3 min ago". The 1-unit case is explicitly singular ("1 min
// ago", not "1 mins ago") because the action-items page surfaces
// this string at list-row density and a missing singularisation
// reads as sloppy.

export function formatRelativeTime(input: Date | string): string {
  const d =
    typeof input === "string" ? new Date(input) : input;
  if (Number.isNaN(d.getTime())) return "";
  const now = Date.now();
  const deltaMs = now - d.getTime();
  const deltaSec = Math.floor(deltaMs / 1000);

  // Future or barely-past → "just now". The absolute-date fallback
  // covers anything more than a year out in either direction.
  if (deltaSec < 60 && deltaSec >= -60) return "just now";

  if (deltaSec >= 60 && deltaSec < 3600) {
    const m = Math.floor(deltaSec / 60);
    return `${m} ${m === 1 ? "min" : "mins"} ago`;
  }
  if (deltaSec >= 3600 && deltaSec < 86400) {
    const h = Math.floor(deltaSec / 3600);
    return `${h} ${h === 1 ? "hr" : "hrs"} ago`;
  }
  if (deltaSec >= 86400 && deltaSec < 604800) {
    const days = Math.floor(deltaSec / 86400);
    return `${days} ${days === 1 ? "day" : "days"} ago`;
  }

  // < 1y (counted in seconds) renders without year; >= 1y renders
  // with year. 31_536_000 is 365d; leap-year drift is cosmetically
  // irrelevant at this scale (the user sees "Dec 31 2024" vs
  // "Dec 31 2025", both correct).
  const hasYear = Math.abs(deltaSec) >= 31_536_000;
  const opts: Intl.DateTimeFormatOptions = hasYear
    ? { month: "short", day: "numeric", year: "numeric" }
    : { month: "short", day: "numeric" };
  return d.toLocaleDateString("en-US", opts);
}
