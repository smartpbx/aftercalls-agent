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
