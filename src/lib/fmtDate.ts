// Date-formatting helpers — agent mirror of `portal/src/lib/fmtDate.ts`
// added for #592 (portal `/settings/privacy` parity). The portal
// module documents this as portal-only; the agent now consumes the
// `fmtDateTimeLocal` shape on the privacy page, so we mirror just
// what's needed here. When the umbrella `packages/shared` lift
// (#355 / #446) lands, this should fold into the shared module.

/** Short month+day+time: "Apr 21, 3:42 PM". A compact variant used
 *  on the privacy page for export rows + access-log timestamps where
 *  the weekday adds clutter. */
export function fmtDateTimeLocal(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}
