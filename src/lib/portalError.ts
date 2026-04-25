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
