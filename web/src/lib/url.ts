/**
 * The one check before a URL that came from outside — an agent's tool call, a
 * GitHub record, a server-supplied address — becomes an `href`.
 *
 * React 18 only *warns* about `javascript:` URLs and renders them anyway, so a
 * prompt-injected page that talks a model into fetching one would leave a
 * link on the dashboard that runs script on this origin when clicked. Only
 * `http:` and `https:` make it through; anything else — `javascript:`,
 * `data:`, `vbscript:`, a relative path, an empty string — is `undefined`,
 * which on an anchor is "no link", not a broken one.
 */
export function safeHttpUrl(raw: unknown): string | undefined {
  if (typeof raw !== "string" || raw.trim() === "") return undefined;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return undefined;
  }
  return url.protocol === "http:" || url.protocol === "https:" ? url.href : undefined;
}
