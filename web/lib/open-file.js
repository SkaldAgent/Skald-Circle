/**
 * Global file-opener helper.
 *
 * `window.openFile(path)` is the single entry point for "show this file to the
 * user in the file-viewer page". It navigates to
 * `#file_viewer?path=<encodeURIComponent(path)>`, which the hash router in
 * `sidebar.js` resolves to the `<file-viewer-page>` element. Back/forward
 * browser navigation works naturally.
 *
 * Components that want to open a file should call `openFile(path)` rather than
 * set the hash directly — this keeps the URL format in one place.
 *
 * Agent-driven opening (the future `show_file_to_user` tool) will set the same
 * hash from the WS payload, so manual and agent-driven paths funnel together.
 */
export function openFile(path) {
  if (!path) return;
  location.hash = `file_viewer?path=${encodeURIComponent(path)}`;
}

window.openFile = openFile;
