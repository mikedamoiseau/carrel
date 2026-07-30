// Highlight Exporter — example Carrel plugin.
//
// Reacts to HighlightCreated and appends the new highlight to a Markdown
// file named after the book, inside the export folder you chose when you
// enabled the plugin. The output is Obsidian-vault friendly.
//
// Event payload (HighlightCreated): `book_id`, `highlight_id`.
//
// Host functions (granted via plugin.toml):
//   get_book(id)            -> book map, or () if not found   [read:library]
//   get_highlights(book_id) -> array of highlight maps        [read:highlights]
//   append_file(rel, text)  -> append to {export_dir}/rel     [write:files]
//
// A highlight map has: id, book_id, chapter_index, text, color, note (or ()).

fn slugify(name) {
    let out = "";
    for ch in name {
        // Keep it filesystem-safe: letters, digits, space, dash, underscore.
        if ch.is_alphanumeric() || ch == ' ' || ch == '-' || ch == '_' {
            out += ch;
        } else {
            out += "-";
        }
    }
    if out == "" { out = "untitled" }
    out
}

fn on_event(event) {
    let book = get_book(event.book_id);
    if book == () { return; }

    // Find the highlight that was just created.
    let highlights = get_highlights(event.book_id);
    let current = ();
    for h in highlights {
        if h.id == event.highlight_id {
            current = h;
            break;
        }
    }
    if current == () { return; }

    let file = slugify(book.title) + ".md";

    // One Markdown blockquote per highlight, plus the note if present.
    let entry = "> " + current.text + "\n";
    if current.note != () {
        entry += "\n" + current.note + "\n";
    }
    entry += "\n— " + book.title + " (" + book.author + ")\n\n";

    append_file(file, entry);
}
