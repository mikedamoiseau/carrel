#!/usr/bin/env python3
"""Build Carrel's offline dictionary artifact from a WordNet 3.1 (WNdb-3.1) tarball.

Reads the `dict/` payload of `wn3.1.dict.tar.gz` and emits a self-contained,
read-only SQLite database with the schema documented in
`carrel-core/src/dictionary.rs`. Only single-word lemmas are kept — multiword
collocations (~40% of WordNet entries) are unreachable by the reader's
single-word lookup, and semantic relations are dropped entirely.

stdlib only (`sqlite3`, `tarfile`, ...). Deterministic: rows are inserted in
sorted order and the DB is `VACUUM`ed, so re-running on the same input yields a
byte-identical file (and therefore a stable gzip checksum).

Usage:
    python3 build_dictionary_db.py <wn3.1.dict.tar.gz> <output.db>
"""

import os
import sqlite3
import sys
import tarfile

SCHEMA_VERSION = "1"
WORDNET_VERSION = "3.1"

# WordNet ss_type -> our POS. 's' (satellite adjective) folds into 'a'.
POS_MAP = {"n": "n", "v": "v", "a": "a", "s": "a", "r": "r"}
DATA_FILES = {"n": "data.noun", "v": "data.verb", "a": "data.adj", "r": "data.adv"}
INDEX_FILES = {"n": "index.noun", "v": "index.verb", "a": "index.adj", "r": "index.adv"}
EXC_FILES = {"n": "noun.exc", "v": "verb.exc", "a": "adj.exc", "r": "adv.exc"}

SCHEMA = """
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE words (id INTEGER PRIMARY KEY, word TEXT NOT NULL UNIQUE);
CREATE TABLE senses (
  id INTEGER PRIMARY KEY,
  word_id INTEGER NOT NULL REFERENCES words(id),
  pos TEXT NOT NULL,
  sense_num INTEGER NOT NULL,
  gloss TEXT NOT NULL,
  examples TEXT,
  synonyms TEXT
);
CREATE INDEX idx_senses_word ON senses(word_id, pos, sense_num);
CREATE TABLE lemma_exceptions (
  form TEXT, pos TEXT, lemma TEXT,
  PRIMARY KEY (form, pos, lemma)
) WITHOUT ROWID;
"""

LICENSE_TEXT = (
    "WordNet 3.1 Copyright 2011 by Princeton University. All rights reserved.\n"
    "WordNet is made available under the WordNet License. Definitions in this\n"
    "artifact are derived from WordNet 3.1 and used under that license.\n"
    "See https://wordnet.princeton.edu/license-and-commercial-use"
)


def _is_single_word(lemma):
    # Collocations use '_' for spaces; keep only genuine single words.
    return "_" not in lemma and lemma != ""


def _parse_gloss(gloss):
    """Split a WordNet gloss into (definition, [examples]).

    Format is `definition; "example"; "example"`. Examples are the quoted
    segments; the definition is everything before the first quote.
    """
    gloss = gloss.strip()
    definition = gloss
    examples = []
    quote = gloss.find('"')
    if quote != -1:
        definition = gloss[:quote].rstrip(" ;").strip()
        rest = gloss[quote:]
        # Pull out each "..."-quoted example.
        segment = ""
        in_quote = False
        for ch in rest:
            if ch == '"':
                if in_quote:
                    text = segment.strip()
                    if text:
                        examples.append(text)
                    segment = ""
                in_quote = not in_quote
            elif in_quote:
                segment += ch
    return definition, examples


def parse_data_file(fh):
    """Parse a `data.<pos>` file -> {offset: (words, definition, examples)}.

    `words` is the list of single-word lemmas in the synset (lowercased).
    """
    synsets = {}
    for raw in fh:
        line = raw.rstrip("\n")
        if line.startswith("  ") or not line:
            continue  # license header lines are space-indented
        gloss_split = line.split(" | ", 1)
        columns = gloss_split[0].split()
        gloss = gloss_split[1] if len(gloss_split) > 1 else ""
        try:
            offset = columns[0]
            w_cnt = int(columns[3], 16)
        except (IndexError, ValueError):
            continue
        words = []
        idx = 4
        for _ in range(w_cnt):
            lemma = columns[idx].lower()
            if _is_single_word(lemma):
                words.append(lemma)
            idx += 2  # skip lex_id following each word
        definition, examples = _parse_gloss(gloss)
        synsets[offset] = (words, definition, examples)
    return synsets


def parse_index_file(fh):
    """Parse an `index.<pos>` file -> {lemma: [offset, ...]} in sense order."""
    order = {}
    for raw in fh:
        line = raw.rstrip("\n")
        if line.startswith("  ") or not line:
            continue
        columns = line.split()
        lemma = columns[0].lower()
        if not _is_single_word(lemma):
            continue
        try:
            p_cnt = int(columns[3])
        except (IndexError, ValueError):
            continue
        # columns: lemma pos synset_cnt p_cnt [ptr_symbol x p_cnt]
        #          sense_cnt tagsense_cnt [synset_offset x synset_cnt]
        # Offsets begin after the p_cnt pointer symbols plus sense_cnt and
        # tagsense_cnt (two fields).
        offsets = columns[4 + p_cnt + 2 :]
        order[lemma] = offsets
    return order


def parse_exc_file(fh):
    """Parse a `<pos>.exc` file -> list of (form, lemma) pairs."""
    pairs = []
    for raw in fh:
        parts = raw.split()
        if len(parts) < 2:
            continue
        form = parts[0].lower()
        for lemma in parts[1:]:
            pairs.append((form, lemma.lower()))
    return pairs


def _open_member(tar, name):
    # Members may be `dict/<name>` or `<name>` depending on how the tarball
    # was packed; try both.
    for candidate in (f"dict/{name}", name):
        try:
            member = tar.getmember(candidate)
        except KeyError:
            continue
        return tar.extractfile(member)
    raise SystemExit(f"error: {name} not found in tarball")


def build(tar_path, out_path):
    if os.path.exists(out_path):
        os.remove(out_path)

    # (word, pos, sense_num) -> (gloss, examples, synonyms)
    entries = {}
    exceptions = set()

    with tarfile.open(tar_path, "r:gz") as tar:
        for pos, data_name in DATA_FILES.items():
            with _open_member(tar, data_name) as raw:
                text = raw.read().decode("latin-1")
            synsets = parse_data_file(text.splitlines(keepends=True))

            with _open_member(tar, INDEX_FILES[pos]) as raw:
                itext = raw.read().decode("latin-1")
            order = parse_index_file(itext.splitlines(keepends=True))

            for lemma, offsets in order.items():
                sense_num = 0
                for offset in offsets:
                    synset = synsets.get(offset)
                    if synset is None:
                        continue
                    words, definition, examples = synset
                    if not definition:
                        continue
                    sense_num += 1
                    synonyms = [w for w in words if w != lemma]
                    entries[(lemma, pos, sense_num)] = (
                        definition,
                        examples,
                        synonyms,
                    )

            with _open_member(tar, EXC_FILES[pos]) as raw:
                etext = raw.read().decode("latin-1")
            for form, lemma in parse_exc_file(etext.splitlines(keepends=True)):
                if _is_single_word(form) and _is_single_word(lemma):
                    exceptions.add((form, pos, lemma))

    conn = sqlite3.connect(out_path)
    try:
        conn.executescript(SCHEMA)
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?)",
            ("schema_version", SCHEMA_VERSION),
        )
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?)",
            ("wordnet_version", WORDNET_VERSION),
        )
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?)", ("license", LICENSE_TEXT)
        )

        # Deterministic word ids: assign in sorted lemma order.
        all_words = sorted({key[0] for key in entries})
        word_ids = {}
        for i, word in enumerate(all_words, start=1):
            word_ids[word] = i
            conn.execute("INSERT INTO words (id, word) VALUES (?, ?)", (i, word))

        for key in sorted(entries):
            lemma, pos, sense_num = key
            definition, examples, synonyms = entries[key]
            conn.execute(
                "INSERT INTO senses (word_id, pos, sense_num, gloss, examples, synonyms) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (
                    word_ids[lemma],
                    pos,
                    sense_num,
                    definition,
                    "\n".join(examples) if examples else None,
                    "\n".join(synonyms) if synonyms else None,
                ),
            )

        for row in sorted(exceptions):
            conn.execute(
                "INSERT OR IGNORE INTO lemma_exceptions (form, pos, lemma) VALUES (?, ?, ?)",
                row,
            )

        conn.commit()
        conn.execute("VACUUM")
        conn.commit()

        n_words = conn.execute("SELECT COUNT(*) FROM words").fetchone()[0]
        n_senses = conn.execute("SELECT COUNT(*) FROM senses").fetchone()[0]
        n_exc = conn.execute("SELECT COUNT(*) FROM lemma_exceptions").fetchone()[0]
    finally:
        conn.close()

    print(f"  words: {n_words}, senses: {n_senses}, exceptions: {n_exc}")


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: build_dictionary_db.py <wn3.1.dict.tar.gz> <output.db>")
    build(sys.argv[1], sys.argv[2])


if __name__ == "__main__":
    main()
