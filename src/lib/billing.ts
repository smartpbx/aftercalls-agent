// Shared billing helpers (#602/WS-G).
//
// The desktop app does NOT own billing management — subscribe / manage
// / update-card all live on the web portal, whose Organization tab
// hosts the Stripe-backed billing card. The app's job is to surface
// status and send the user to that page in the system browser.
//
// One canonical URL so the Settings subscription card and the Record
// page's subscription-gate block never drift.

import { openUrl } from "@tauri-apps/plugin-opener";

/** Portal Organization tab — the billing section lives here (subscribe,
 *  manage, update payment). Verified against
 *  `portal/src/routes/admin/+page.svelte` (`?section=organization`). */
export const BILLING_URL = "https://app.aftercalls.io/admin?section=organization";

/** Open the portal billing page in the system browser. Best-effort —
 *  a failed `openUrl` is logged, never thrown into a render path. */
export async function openBilling(): Promise<void> {
  try {
    await openUrl(BILLING_URL);
  } catch (e) {
    console.warn("openUrl (billing) failed", e);
  }
}
