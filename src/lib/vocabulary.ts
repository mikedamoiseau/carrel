// Pure, framework-free helpers for the vocabulary builder (F-1-5).
//
// The backend (carrel-core) owns the Leitner scheduling math and CRUD; this
// module owns the small pieces of client-side logic that benefit from being
// unit-tested in isolation:
//   - `extractContextSentence` — derives the surrounding sentence from the
//     chapter's plain text and the selection's character offsets, so the
//     vocabulary row captures more context than the raw selected word.
//   - `formatDefinitionSnapshot` — renders a `DictionaryEntry` (from
//     `src/lib/dictionary.ts`) into the single string stored as the row's
//     `definition` snapshot at log time.
//   - `boxIntervalDays` / `vocabularyPosLabelKey` — small display helpers for
//     the Vocabulary screen.

import { groupSensesByPos, POS_LABEL_KEYS, type DictionaryEntry, type DictionarySense } from "./dictionary";

/**
 * A saved vocabulary row, as returned by `list_vocabulary` / `get_due_vocabulary`
 * (JSON keys match the backend's `VocabularyWord` serialization exactly).
 */
export interface VocabularyWord {
  id: string;
  lemma: string;
  word: string;
  pos: string | null;
  definition: string;
  bookId: string | null;
  bookTitle: string | null;
  chapterIndex: number | null;
  contextSentence: string | null;
  startOffset: number | null;
  endOffset: number | null;
  seenCount: number;
  box: number;
  lastReviewedAt: number | null;
  nextDueAt: number | null;
  lastSeenAt: number;
  createdAt: number;
}

/** Max length (roughly, pre-collapse) of the stored context sentence. */
const MAX_CONTEXT_CHARS = 300;

/** Max synonyms folded into the definition snapshot. */
const MAX_SNAPSHOT_SYNONYMS = 3;

/**
 * Derive the sentence surrounding a text selection from the full chapter
 * plain text and the selection's [startOffset, endOffset) character range.
 *
 * Expands outward from the selection to the nearest sentence boundary: a
 * run of `.`/`?`/`!` followed by whitespace, a newline/paragraph break, or
 * the start/end of the text. Collapses internal whitespace runs to single
 * spaces and caps the result at ~300 chars — when the sentence itself is
 * longer, a window around the selection is kept instead of truncating from
 * the start.
 *
 * Returns "" for empty input or out-of-range/nonsensical offsets — the
 * caller (the reader's auto-log hook) treats that as "no context available"
 * rather than a fatal error.
 */
export function extractContextSentence(
  chapterText: string,
  startOffset: number,
  endOffset: number,
): string {
  if (chapterText === "") return "";
  if (
    !Number.isFinite(startOffset) ||
    !Number.isFinite(endOffset) ||
    startOffset < 0 ||
    endOffset < 0 ||
    startOffset > endOffset ||
    endOffset > chapterText.length
  ) {
    return "";
  }

  // Boundary = end of a "[.?!]+ whitespace" run, or end of a newline run
  // (paragraph break), collected as cut points alongside the text's start
  // and end.
  const boundaries: number[] = [0];
  const boundaryRegex = /[.!?]+\s+|\n+/g;
  let match: RegExpExecArray | null;
  while ((match = boundaryRegex.exec(chapterText)) !== null) {
    boundaries.push(match.index + match[0].length);
  }
  boundaries.push(chapterText.length);

  let sentenceStart = 0;
  for (const b of boundaries) {
    if (b <= startOffset) sentenceStart = b;
    else break;
  }
  let sentenceEnd = chapterText.length;
  for (const b of boundaries) {
    if (b >= endOffset) {
      sentenceEnd = b;
      break;
    }
  }

  let windowStart = sentenceStart;
  let windowEnd = sentenceEnd;
  const selectionLen = endOffset - startOffset;

  if (sentenceEnd - sentenceStart > MAX_CONTEXT_CHARS) {
    if (selectionLen >= MAX_CONTEXT_CHARS) {
      windowStart = startOffset;
      windowEnd = Math.min(sentenceEnd, startOffset + MAX_CONTEXT_CHARS);
    } else {
      const extra = MAX_CONTEXT_CHARS - selectionLen;
      const before = Math.floor(extra / 2);
      const after = extra - before;
      windowStart = Math.max(sentenceStart, startOffset - before);
      windowEnd = Math.min(sentenceEnd, endOffset + after);
      // If clipped on one side (selection near a sentence edge), spend the
      // unused budget extending the other side, so the window still uses
      // close to the full cap where the sentence allows it.
      let deficit = MAX_CONTEXT_CHARS - (windowEnd - windowStart);
      if (deficit > 0 && windowStart === sentenceStart) {
        windowEnd = Math.min(sentenceEnd, windowEnd + deficit);
      }
      deficit = MAX_CONTEXT_CHARS - (windowEnd - windowStart);
      if (deficit > 0 && windowEnd === sentenceEnd) {
        windowStart = Math.max(sentenceStart, windowStart - deficit);
      }
    }
  }

  const raw = chapterText.slice(windowStart, windowEnd);
  const collapsed = raw.trim().replace(/\s+/g, " ");
  return collapsed;
}

/**
 * The sense the reader's definition card shows first: the first sense of
 * `groupSensesByPos(entry.senses)`'s first (POS-ordered) group. This can
 * differ from raw `entry.senses[0]` when that sense's POS isn't first in
 * the card's fixed n/v/a/r display order — callers that need to mirror what
 * the user actually sees as the "primary" sense (the vocabulary snapshot,
 * its logged `pos`) should derive it from here rather than indexing
 * `entry.senses` directly.
 */
export function primarySense(entry: DictionaryEntry): DictionarySense | undefined {
  return groupSensesByPos(entry.senses)[0]?.senses[0];
}

/**
 * Render a `DictionaryEntry`'s primary sense (see `primarySense`, which
 * mirrors the reader's definition card) into the single string stored as
 * the vocabulary row's `definition` snapshot: the gloss, plus up to a small
 * number of synonyms. Self-contained — no live re-lookup is needed to
 * review a saved word later.
 */
export function formatDefinitionSnapshot(entry: DictionaryEntry): string {
  const primary = primarySense(entry);
  if (!primary) return "";
  if (primary.synonyms.length === 0) return primary.gloss;
  const synonyms = primary.synonyms.slice(0, MAX_SNAPSHOT_SYNONYMS).join(", ");
  return `${primary.gloss} (${synonyms})`;
}

/** Leitner box intervals (days), boxes 1..5 — mirrors carrel-core's schedule. */
const BOX_INTERVALS_DAYS = [1, 3, 7, 14, 30];

/** Review interval, in days, for a given Leitner box (clamped to 1..5). */
export function boxIntervalDays(box: number): number {
  const clamped = Math.min(Math.max(Math.round(box), 1), BOX_INTERVALS_DAYS.length);
  return BOX_INTERVALS_DAYS[clamped - 1];
}

/** i18n key for a part-of-speech label, or `null` when `pos` is absent/unknown. */
export function vocabularyPosLabelKey(pos: string | null): string | null {
  if (!pos) return null;
  return POS_LABEL_KEYS[pos] ?? null;
}

/**
 * Client-side filter predicate for the vocabulary list's search box.
 *
 * Case-insensitive substring match across `word`, `lemma`, `definition`, and
 * `bookTitle`. An empty (or whitespace-only) query matches everything, so
 * clearing the search box restores the full list.
 */
export function matchesVocabularyQuery(word: VocabularyWord, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (needle === "") return true;
  return (
    word.word.toLowerCase().includes(needle) ||
    word.lemma.toLowerCase().includes(needle) ||
    word.definition.toLowerCase().includes(needle) ||
    (word.bookTitle?.toLowerCase().includes(needle) ?? false)
  );
}
