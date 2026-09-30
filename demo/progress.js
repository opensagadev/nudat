// Work progress, not a time estimate. Reserve the last percent for final ZIP close().
export function exportProgress({ total, decoded, written, completed }, fileCount) {
  if (fileCount && completed === fileCount) return 0.99;
  if (!fileCount) return 0;
  if (!total) return 0.99 * completed / fileCount;
  const clamp = n => Math.max(0, Math.min(1, n));
  return Math.min(0.99, 0.5 * clamp(decoded / total) + 0.49 * clamp(written / total));
}
