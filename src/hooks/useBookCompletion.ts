import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";

const DISMISSED_KEY = "carrel-celebration-dismissed";

interface BookInfo {
  title: string;
  author: string;
  cover_path: string | null;
}

export interface CompletionPayload {
  bookId: string;
  title: string;
  author: string;
  coverPath: string | null;
  totalChapters: number;
}

export interface UseBookCompletion {
  showCelebration: boolean;
  completionData: CompletionPayload | null;
  readingTimeSecs: number;
  dismiss: () => void;
}

function getDismissedBooks(): string[] {
  try {
    return JSON.parse(localStorage.getItem(DISMISSED_KEY) || "[]");
  } catch {
    return [];
  }
}

function isDismissed(bookId: string): boolean {
  return getDismissedBooks().includes(bookId);
}

function markDismissed(bookId: string): void {
  const dismissed = getDismissedBooks();
  if (!dismissed.includes(bookId)) {
    dismissed.push(bookId);
    localStorage.setItem(DISMISSED_KEY, JSON.stringify(dismissed));
  }
}

export function useBookCompletion(
  bookId: string,
  chapterIndex: number,
  totalChapters: number,
): UseBookCompletion {
  const [completionData, setCompletionData] = useState<CompletionPayload | null>(null);
  const [readingTimeSecs, setReadingTimeSecs] = useState(0);
  const [dismissed, setDismissed] = useState(() => isDismissed(bookId));
  const prevChapterRef = useRef(chapterIndex);
  const initializedRef = useRef(false);

  useEffect(() => {
    if (!initializedRef.current) {
      initializedRef.current = true;
      prevChapterRef.current = chapterIndex;
      return;
    }

    const wasOnLast = totalChapters > 0 && prevChapterRef.current >= totalChapters - 1;
    const nowOnLast = totalChapters > 0 && chapterIndex >= totalChapters - 1;
    prevChapterRef.current = chapterIndex;

    if (nowOnLast && !wasOnLast && !isDismissed(bookId)) {
      invoke<BookInfo>("get_book", { bookId })
        .then((book) => {
          setCompletionData({
            bookId,
            title: book.title,
            author: book.author,
            coverPath: book.cover_path,
            totalChapters,
          });
        })
        .catch(() => {});
      invoke<number>("get_book_reading_time", { bookId })
        .then(setReadingTimeSecs)
        .catch(() => setReadingTimeSecs(0));
    }
  }, [bookId, chapterIndex, totalChapters]);

  const showCelebration = !dismissed && completionData !== null;

  const dismiss = useCallback(() => {
    markDismissed(bookId);
    setDismissed(true);
    setCompletionData(null);
  }, [bookId]);

  return {
    showCelebration,
    completionData,
    readingTimeSecs,
    dismiss,
  };
}
