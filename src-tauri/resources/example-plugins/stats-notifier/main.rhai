// Stats Notifier — example Carrel plugin.
//
// Reacts to BookFinished (fired when you reach the last chapter of a book)
// and shows a desktop notification.
//
// The BookFinished event payload has `book_id`. We look the book up to get
// its title.
//
// Host functions (granted via plugin.toml):
//   get_book(id)           -> book map, or () if not found   [read:library]
//   notify(title, body)    -> desktop notification           [notify]

fn on_event(event) {
    let book = get_book(event.book_id);
    if book == () {
        return;
    }

    notify("Book finished", "You finished \"" + book.title + "\". Nice work!");
}
