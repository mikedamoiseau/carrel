/**
 * Pure helpers for split-view state and its localStorage shape.
 *
 * The Reader screen mounts one ReaderPane per side and tracks
 * persistence with two keys per primary book:
 *   - `carrel-split-mode-<bookId>`        — "1" if split is on
 *   - `carrel-split-companion-<bookId>`   — companion bookId
 *
 * Keeping this contract here (instead of inline in `Reader.tsx`) makes
 * the storage transitions easy to test without spinning up React.
 */

export const SPLIT_MODE_PREFIX = "carrel-split-mode-";
export const SPLIT_COMPANION_PREFIX = "carrel-split-companion-";

export const splitModeKey = (bookId: string) => `${SPLIT_MODE_PREFIX}${bookId}`;
export const splitCompanionKey = (bookId: string) =>
  `${SPLIT_COMPANION_PREFIX}${bookId}`;

export interface SplitState {
  splitMode: boolean;
  companionBookId: string | null;
}

export function readSplitState(storage: Storage, bookId: string): SplitState {
  return {
    splitMode: storage.getItem(splitModeKey(bookId)) === "1",
    companionBookId: storage.getItem(splitCompanionKey(bookId)),
  };
}

export function writeSplitMode(
  storage: Storage,
  bookId: string,
  on: boolean,
): void {
  if (on) storage.setItem(splitModeKey(bookId), "1");
  else storage.removeItem(splitModeKey(bookId));
}

export function writeCompanion(
  storage: Storage,
  bookId: string,
  companionId: string | null,
): void {
  if (companionId) storage.setItem(splitCompanionKey(bookId), companionId);
  else storage.removeItem(splitCompanionKey(bookId));
}

/**
 * Storage transitions for swapping panes.
 *
 * The URL's `:bookId` is canonical primary, so a swap navigates to the
 * companion book and seeds *its* split state with the old primary as
 * companion. The old primary's pairing is left intact so navigating
 * back later restores the same split layout.
 */
export function applySwap(
  storage: Storage,
  oldPrimary: string,
  companion: string,
): void {
  storage.setItem(splitModeKey(companion), "1");
  storage.setItem(splitCompanionKey(companion), oldPrimary);
}

/** Effective companion id with same-book fallback. */
export function effectiveCompanionId(
  companionBookId: string | null,
  bookId: string,
): string {
  return companionBookId ?? bookId;
}

/**
 * Whether the companion pane should persist its own progress. False
 * when both panes happen to show the same book (then only the primary
 * writes, to avoid two panes racing on the same DB row).
 */
export function canPersistCompanion(
  companionBookId: string | null,
  bookId: string,
): boolean {
  return effectiveCompanionId(companionBookId, bookId) !== bookId;
}
