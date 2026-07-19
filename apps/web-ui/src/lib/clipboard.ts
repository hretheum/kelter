/* Copy affordance (design/04 §5): raw value to clipboard; object → 2-space
 * JSON, string as-is. Returns false when the Clipboard API is unavailable so
 * the caller can show the select-and-⌘C fallback. */
export async function copyValue(value: unknown): Promise<boolean> {
  const text =
    typeof value === "string" ? value : JSON.stringify(value, null, 2);
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
