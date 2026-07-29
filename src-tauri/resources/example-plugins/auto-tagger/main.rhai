// Auto Tagger — example Carrel plugin.
//
// Carrel calls `on_event(event)` for every event this plugin subscribes to
// (see plugin.toml `[events] subscribe`). Here we react to BookImported.
//
// The `event` map always has a `type` field plus the event's payload. For
// BookImported that is `book_id`, `format` ("epub", "pdf", "cbz", "cbr",
// "mobi"), and `source` ("Manual", "FolderScan", "Download").
//
// Host functions available to this plugin (granted via plugin.toml):
//   get_book(id)              -> book map, or () if not found   [read:library]
//   add_tag(book_id, tag)     -> apply a tag                    [write:tags]
//   remove_tag(book_id, tag)  -> remove a tag                   [write:tags]

fn on_event(event) {
    // Map each format to a human-friendly tag. Edit this to taste.
    let tag = if event.format == "cbz" || event.format == "cbr" {
        "comic"
    } else if event.format == "pdf" {
        "pdf"
    } else if event.format == "mobi" {
        "kindle"
    } else {
        "ebook"
    };

    add_tag(event.book_id, tag);

    // Books pulled in from a catalog/URL get an extra tag so they are easy
    // to find later. Delete this block if you don't want it.
    if event.source == "Download" {
        add_tag(event.book_id, "from-catalog");
    }
}
