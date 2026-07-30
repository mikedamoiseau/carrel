/** Client-side sanitization for EPUB chapter HTML (defense-in-depth). */
import DOMPurify from "dompurify";

// DOMPurify's default URI allowlist rejects custom schemes, which would strip
// the reader's own `asset://localhost/...` image sources (see
// `rewrite_img_srcs_to_asset_urls` in carrel-core). This is the upstream default
// with `asset` added to the scheme alternation; nothing else is loosened.
const ALLOWED_URI_REGEXP =
  /^(?:(?:(?:f|ht)tps?|mailto|tel|callto|sms|cid|xmpp|matrix|asset):|[^a-z]|[a-z+.\-]+(?:[^a-z+.\-:]|$))/i;

/**
 * Sanitize already-server-sanitized (ammonia) EPUB chapter HTML before it is
 * injected via `dangerouslySetInnerHTML`. Second layer behind the server; must
 * strip active content (scripts, event handlers, `javascript:` URLs) while
 * preserving the reader's own `asset://` image URLs and highlight `<mark>`s.
 */
export function sanitizeChapterHtml(html: string): string {
  return DOMPurify.sanitize(html, { ALLOWED_URI_REGEXP });
}
