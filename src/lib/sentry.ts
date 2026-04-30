import * as Sentry from "@sentry/browser";

let initialized = false;
let telemetryAllowed = false;

export function setSentryTelemetryAllowed(enabled: boolean): void {
  telemetryAllowed = enabled;
  if (enabled) initSentry(true);
}

export function initSentry(enabled: boolean): void {
  telemetryAllowed = enabled;
  const dsn = import.meta.env.VITE_SENTRY_DSN;
  if (!dsn || initialized || !telemetryAllowed) return;
  initialized = true;

  Sentry.init({
    dsn,
    environment:
      import.meta.env.VITE_SENTRY_ENVIRONMENT ?? import.meta.env.MODE,
    release:
      import.meta.env.VITE_SENTRY_RELEASE ??
      import.meta.env.VITE_BUILD_TAG ??
      import.meta.env.VITE_BUILD_SHA,
    sendDefaultPii: false,
    tracesSampleRate: 0,
    beforeBreadcrumb: () => null,
    beforeSend: (event) => (telemetryAllowed ? sanitizeEvent(event) : null),
  });
}

function sanitizeEvent(event: any): any {
  event.user = undefined;
  if (event.request) {
    event.request.url = scrubUrl(event.request.url);
    event.request.headers = undefined;
    event.request.cookies = undefined;
    event.request.data = undefined;
    event.request.query_string = undefined;
  }
  event.message = scrubText(event.message);
  event.transaction = scrubUrl(event.transaction);
  event.extra = scrubRecord(event.extra);
  event.contexts = scrubRecord(event.contexts);
  event.breadcrumbs = undefined;

  const values = event.exception?.values;
  if (Array.isArray(values)) {
    for (const exc of values) {
      exc.value = scrubText(exc.value);
      exc.module = scrubText(exc.module);
    }
  }

  return event;
}

const EMAIL_RE = /\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const UUID_RE =
  /\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b/gi;
const TOKEN_RE = /\b[A-Za-z0-9_-]{24,}\.[A-Za-z0-9_-]{12,}\.[A-Za-z0-9_-]{12,}\b/g;
const SENSITIVE_KEY_RE =
  /(authorization|cookie|token|secret|password|transcript|summary|notes?|audio|body|request|response|content|markdown|html)/i;

function scrubRecord(value: unknown, depth = 0): unknown {
  if (!value || depth > 2) return undefined;
  if (typeof value === "string") return scrubText(value);
  if (typeof value !== "object") return value;
  if (Array.isArray(value)) return value.slice(0, 10).map((v) => scrubRecord(v, depth + 1));

  const out: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
    out[key] = SENSITIVE_KEY_RE.test(key)
      ? "[redacted]"
      : scrubRecord(item, depth + 1);
  }
  return out;
}

function scrubUrl(input: unknown): unknown {
  if (typeof input !== "string") return input;
  try {
    const url = new URL(input, window.location.origin);
    url.username = "";
    url.password = "";
    url.search = "";
    url.hash = "";
    url.pathname = url.pathname.replace(UUID_RE, "[uuid]");
    return url.toString();
  } catch {
    return scrubText(input.split("?")[0] ?? input);
  }
}

function scrubText(input: unknown): unknown {
  if (typeof input !== "string") return input;
  const scrubbed = input
    .replace(EMAIL_RE, "[email]")
    .replace(UUID_RE, "[uuid]")
    .replace(TOKEN_RE, "[token]");
  return scrubbed.length > 1_000
    ? `${scrubbed.slice(0, 1_000)}...[truncated]`
    : scrubbed;
}
