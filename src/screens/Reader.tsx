import { useCallback, useEffect, useRef, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import ReaderPane from "../components/ReaderPane";
import BookPickerModal from "../components/BookPickerModal";
import {
  applySwap,
  canPersistCompanion,
  effectiveCompanionId,
  readSplitState,
  writeCompanion,
  writeSplitMode,
} from "../lib/splitView";

interface ReaderProps {
  onOpenSettings: () => void;
  settingsOpen?: boolean;
}

/**
 * Reader screen — layout shell that mounts the per-book reading view.
 *
 * Single-pane today by default; split view mounts two
 * panes side-by-side at a fixed 50/50 split. The user picks the
 * companion book through the BookPickerModal opened from the
 * companion pane's header. Active-pane focus is tracked here so
 * keyboard navigation routes to whichever pane the user last clicked.
 *
 * Persistence: split mode and the companion bookId both persist per
 * primary book in `localStorage` so reopening restores the layout +
 * pairing.
 */
export default function Reader({ onOpenSettings, settingsOpen = false }: ReaderProps) {
  const { bookId } = useParams<{ bookId: string }>();
  const navigate = useNavigate();
  const location = useLocation();
  const locState = location.state as { chapterIndex?: number; autoFocus?: boolean; offset?: number | null } | null;
  const incomingChapter = locState?.chapterIndex ?? undefined;
  const incomingOffset = locState?.offset ?? null;
  const autoFocus = locState?.autoFocus ?? false;
  // Tracks the `location.key` of the last-consumed navigation (rather than
  // `bookId`) so a jump to a book that's *already open* — same pathname, no
  // remount, `bookId` unchanged — still gets applied. Each `navigate()` call
  // (including the state-clearing one below) mints a fresh `location.key`,
  // so this correctly fires once per distinct navigation regardless of
  // whether the target book changed.
  const lastConsumedKey = useRef<string | null>(null);
  const isFreshJump = incomingChapter !== undefined && lastConsumedKey.current !== location.key;

  useEffect(() => {
    if (isFreshJump) {
      lastConsumedKey.current = location.key;
      navigate(location.pathname, { replace: true, state: {} });
    }
  }, [isFreshJump, location.key, navigate, location.pathname]);

  const initialChapterIndex = isFreshJump ? incomingChapter : undefined;
  // Same freshness gate as initialChapterIndex — only fed to ReaderPane on
  // the render where the navigation state is still live, so it's consumed
  // once and doesn't re-fire on subsequent renders.
  const initialScrollOffset = isFreshJump ? incomingOffset : undefined;
  // Identity of this jump for ReaderPane, which stays mounted across
  // same-book jumps: `location.key` on the render that consumes a fresh
  // jump, held stable otherwise so ReaderPane's jump effect doesn't re-fire
  // on ordinary re-renders (e.g. the state-clearing navigate above, or any
  // unrelated re-render — `lastConsumedKey.current` only changes when a new
  // jump is consumed).
  const jumpToken = isFreshJump ? location.key : (lastConsumedKey.current ?? undefined);

  const [splitMode, setSplitMode] = useState(() => {
    if (!bookId) return false;
    return readSplitState(localStorage, bookId).splitMode;
  });
  const [companionBookId, setCompanionBookId] = useState<string | null>(() => {
    if (!bookId) return null;
    return readSplitState(localStorage, bookId).companionBookId;
  });
  const [activePaneId, setActivePaneId] = useState<"primary" | "companion">("primary");
  const [pickerOpen, setPickerOpen] = useState(false);

  // Reload persisted state on bookId change (Reader stays mounted
  // across books, so the initializers above only run on first mount).
  useEffect(() => {
    if (!bookId) {
      setSplitMode(false);
      setCompanionBookId(null);
      setActivePaneId("primary");
      return;
    }
    const state = readSplitState(localStorage, bookId);
    setSplitMode(state.splitMode);
    setCompanionBookId(state.companionBookId);
    setActivePaneId("primary");
  }, [bookId]);

  const toggleSplit = useCallback(() => {
    if (!bookId) return;
    setSplitMode((prev) => {
      const next = !prev;
      writeSplitMode(localStorage, bookId, next);
      return next;
    });
    setActivePaneId("primary");
  }, [bookId]);

  const openPicker = useCallback(() => setPickerOpen(true), []);
  const closePicker = useCallback(() => setPickerOpen(false), []);
  const handleSelectCompanion = useCallback(
    (id: string) => {
      if (!bookId) return;
      setCompanionBookId(id);
      writeCompanion(localStorage, bookId, id);
      setPickerOpen(false);
    },
    [bookId],
  );

  const swapPanes = useCallback(() => {
    if (!bookId || !companionBookId || companionBookId === bookId) return;
    // Treat the URL's `:bookId` as canonical primary, so a true swap
    // navigates to the companion book and seeds the new primary's
    // split state with the old primary as its companion. The
    // useEffect above re-pulls localStorage on bookId change, so the
    // newly-mounted Reader picks the seed up automatically.
    //
    // The old primary's pairing is left intact (`companion-A = B`) so
    // navigating back to A later restores the same split layout
    // instead of degenerating into a same-book split.
    applySwap(localStorage, bookId, companionBookId);
    navigate(`/reader/${companionBookId}`);
  }, [bookId, companionBookId, navigate]);

  const closeCompanion = useCallback(() => {
    if (!bookId) return;
    setSplitMode(false);
    writeSplitMode(localStorage, bookId, false);
    setActivePaneId("primary");
  }, [bookId]);

  if (!bookId) return null;

  const companionId = effectiveCompanionId(companionBookId, bookId);
  const persistCompanionProgress = canPersistCompanion(companionBookId, bookId);
  const sameBook = !persistCompanionProgress;

  if (!splitMode) {
    return (
      <ReaderPane
        bookId={bookId}
        onOpenSettings={onOpenSettings}
        settingsOpen={settingsOpen}
        splitMode={false}
        isPrimary
        onToggleSplit={toggleSplit}
        initialChapterIndex={initialChapterIndex}
        initialScrollOffset={initialScrollOffset}
        jumpToken={jumpToken}
        autoFocus={autoFocus}
      />
    );
  }

  return (
    <>
      <div className="flex flex-row w-full h-full min-h-0">
        <div className="flex-1 min-w-0 border-r border-warm-border">
          <ReaderPane
            bookId={bookId}
            onOpenSettings={onOpenSettings}
            settingsOpen={settingsOpen}
            splitMode
            isPrimary
            isActive={!pickerOpen && activePaneId === "primary"}
            canPersist
            onActivate={() => setActivePaneId("primary")}
            onToggleSplit={toggleSplit}
            onSwapPanes={!sameBook ? swapPanes : undefined}
            initialChapterIndex={initialChapterIndex}
            initialScrollOffset={initialScrollOffset}
            jumpToken={jumpToken}
          />
        </div>
        <div className="flex-1 min-w-0">
          <ReaderPane
            // Key on the companion bookId so React fully remounts the
            // pane (and its per-book state machine) whenever the user
            // picks a different book — same hygiene as a route change.
            key={companionId}
            bookId={companionId}
            onOpenSettings={onOpenSettings}
            settingsOpen={settingsOpen}
            splitMode
            isPrimary={false}
            isActive={!pickerOpen && activePaneId === "companion"}
            // Both panes write their own book's progress, except when
            // they share a bookId (the picker hasn't been used yet) —
            // then only the primary writes to avoid racing on the
            // same DB row.
            canPersist={persistCompanionProgress}
            onActivate={() => setActivePaneId("companion")}
            onToggleSplit={toggleSplit}
            onChangeBook={openPicker}
            onCloseThisPane={closeCompanion}
          />
        </div>
      </div>
      {pickerOpen && (
        <BookPickerModal
          excludeBookId={bookId}
          onSelect={handleSelectCompanion}
          onClose={closePicker}
        />
      )}
    </>
  );
}
