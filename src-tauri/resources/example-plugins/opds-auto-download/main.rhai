// OPDS Auto-Download — example Carrel plugin.
//
// Fetches an OPDS acquisition feed and imports books that aren't already in
// your library. Triggered at app start (AppStarted) and by the "Run now"
// button in Settings → Plugins. v1 has no recurring scheduler.
//
// Host functions (granted via plugin.toml):
//   http_get(url)           -> response body as text   [network]
//   find_books(query)       -> array of book maps       [read:library]
//   import_from_url(url)     -> imports a book           [import:books]
//
// SAFETY: http_get only reaches hosts listed in [permissions.network] of the
// manifest. import_from_url runs the normal import pipeline (dedup included),
// so re-running is safe — already-imported books are skipped by content hash.

const FEED = "https://standardebooks.org/feeds/opds/new-releases";
const MAX_PER_RUN = 5;

fn on_event(event) {
    let xml = http_get(FEED);

    // Minimal OPDS parse: pull EPUB acquisition links out of the Atom feed.
    // (A real plugin might parse titles too; we keep it dependency-free.)
    let imported = 0;
    let rest = xml;
    while imported < MAX_PER_RUN {
        let at = rest.index_of("href=\"");
        if at < 0 { break; }
        rest = rest.sub_string(at + 6);
        let end = rest.index_of("\"");
        if end < 0 { break; }
        let href = rest.sub_string(0, end);
        rest = rest.sub_string(end + 1);

        // Only follow direct EPUB acquisition links.
        if href.ends_with(".epub") {
            import_from_url(href);
            imported += 1;
        }
    }
}
