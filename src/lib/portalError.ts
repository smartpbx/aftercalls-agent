// Structured error shape returned from Tauri IPC commands that hit the
// backend (#124). Mirrors `agent/src-tauri/src/error.rs::PortalError`
// — keep the two in sync when adding variants.
//
// Tauri serialises `Result<T, E>` errors through serde_json when
// `E: Serialize`, so a `throw`n PortalError lands in the JS catch
// block as the plain object below (NOT an Error instance). The `kind`
// discriminator is the stable contract; the optional fields vary per
// variant.

export type PortalError =
  | { kind: "cooldown"; retry_after_seconds: number }
  | { kind: "bad_request"; message: string }
  | { kind: "unauthorized" }
  | { kind: "forbidden" }
  | { kind: "not_found" }
  | { kind: "network"; message: string }
  | { kind: "server"; status: number; message: string }
  | { kind: "other"; message: string };

/// Type guard — narrow an unknown caught value to PortalError. Tauri
/// IPC errors arrive as plain objects; in practice anything caught
/// from a Tauri command that returns `Result<T, PortalError>` will
/// satisfy this, but the guard is defensive in case a runtime bug
/// hands us something else (e.g. a rejected promise from outside the
/// invoke chain).
export function isPortalError(e: unknown): e is PortalError {
  if (typeof e !== "object" || e === null) return false;
  const k = (e as { kind?: unknown }).kind;
  return (
    k === "cooldown" ||
    k === "bad_request" ||
    k === "unauthorized" ||
    k === "forbidden" ||
    k === "not_found" ||
    k === "network" ||
    k === "server" ||
    k === "other"
  );
}

/// Render any caught value (PortalError, Error, string) as a single
/// human-readable string. Used by error banners that don't need to
/// branch on `kind` and just want to surface something the user can
/// read — replaces `String(e)` callsites that would otherwise render
/// `[object Object]` after the #124 sweep migrated commands away from
/// stringified errors.
export function portalErrorToText(e: unknown): string {
  if (isPortalError(e)) {
    switch (e.kind) {
      case "cooldown":
        return `Too many requests — try again in ${e.retry_after_seconds}s.`;
      case "bad_request":
        return e.message || "Request rejected.";
      case "unauthorized":
        return "Not signed in. Please log in again.";
      case "forbidden":
        return "You don't have permission to do that.";
      case "not_found":
        return "Not found.";
      case "network":
        return "Can't reach the server. Check your connection.";
      case "server":
        return e.message || `Server error (${e.status}).`;
      case "other":
        return e.message || "Something went wrong.";
    }
  }
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  // Fallback: avoid `[object Object]` for unexpected shapes.
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}

/// The underlying technical cause, when there is one worth showing —
/// intended for a collapsed "details" disclosure next to the
/// `portalErrorToText` line, not for the banner itself.
///
/// `portalErrorToText` keys its copy on `kind`, which is right for the
/// user-facing sentence but discards the one field that distinguishes
/// causes sharing a kind. Every `network` failure reads "Can't reach
/// the server" whether the real fault is `invalid peer certificate:
/// UnknownIssuer` (a TLS-inspecting proxy — the machine's trust store
/// has a CA we don't), `dns error`, `connection refused`, or a timeout.
/// Those want very different answers from support, and without this the
/// only evidence is a screenshot of identical copy.
///
/// `network` is the only variant that hides anything: every other
/// message-carrying kind (`bad_request`, `server`, `other`) already
/// renders its message — or, for an empty one, copy that states the
/// status — through `portalErrorToText`. A disclosure that repeats the
/// line above it is noise, so those return null.
///
/// CAUTION when adding callsites: this returns a raw transport error,
/// which embeds the URL that failed. That is safe on the login screen
/// (it only ever talks to our own backend) but NOT automatically safe
/// elsewhere — an upload failure would name the object-storage host,
/// putting an infrastructure vendor into user-facing copy and breaking
/// the vendor-opaque rule in CLAUDE.md. Check what host a given caller
/// can fail against before surfacing this.
export function portalErrorDetail(e: unknown): string | null {
  if (!isPortalError(e) || e.kind !== "network") return null;
  return e.message?.trim() || null;
}
