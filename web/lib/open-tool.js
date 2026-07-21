/**
 * Global tool-detail opener helper.
 *
 * `window.openToolDetail(id)` is the single entry point for "show this tool
 * call's full input / result / diff in the dedicated detail page". It navigates
 * to `#tool_detail?id=<id>`, which the hash router in `sidebar.js` resolves to
 * the `<tool-detail-page>` element. Back/forward navigation works naturally.
 *
 * Mirrors `open-file.js` — the URL format lives in one place, so a tool card's
 * "details" (eye) affordance calls this rather than setting the hash directly.
 */
export function openToolDetail(id) {
  if (id == null) return;
  location.hash = `tool_detail?id=${encodeURIComponent(id)}`;
}

window.openToolDetail = openToolDetail;
