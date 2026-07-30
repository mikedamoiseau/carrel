import type { TFunction } from "i18next";

/**
 * Structured error payload produced by the Rust backend (roadmap #55).
 * Every Tauri command now returns this shape when it fails; older/tauri-host
 * errors may arrive as plain strings or Error instances.
 */
export type CarrelErrorPayload = {
  kind:
    | "NotFound"
    | "PermissionDenied"
    | "InvalidInput"
    | "Network"
    | "RateLimited"
    | "Database"
    | "Io"
    | "Serialization"
    | "LockRequired"
    | "Internal"
    | string;
  message: string;
};

function isCarrelErrorPayload(x: unknown): x is CarrelErrorPayload {
  if (!x || typeof x !== "object" || Array.isArray(x)) return false;
  const o = x as Record<string, unknown>;
  return (
    typeof o.kind === "string" &&
    typeof o.message === "string" &&
    o.message.length > 0
  );
}

/** Normalize anything `invoke()` (or any callback) may throw into `{kind?, message}`. */
export function toCarrelError(raw: unknown): { kind?: string; message: string } {
  if (isCarrelErrorPayload(raw)) return { kind: raw.kind, message: raw.message };
  if (typeof raw === "string") return { message: raw };
  if (raw instanceof Error) return { message: raw.message };
  try {
    return { message: String(raw) };
  } catch {
    return { message: "Unknown error" };
  }
}

/** Default translation key per error kind — used when nothing more specific matches. */
const KIND_TO_KEY: Record<string, string> = {
  NotFound: "errors.fileNotFound",
  PermissionDenied: "errors.permissionDenied",
  Network: "errors.networkError",
  RateLimited: "errors.rateLimited",
  InvalidInput: "errors.invalidInput",
  Database: "errors.database",
  Io: "errors.io",
  Serialization: "errors.corrupt",
  LockRequired: "errors.lockRequired",
  Internal: "errors.generic",
};

/**
 * Message-substring → translation key. Kept in sync with Rust variant messages
 * so errors raised before the migration (or by sub-systems that still build
 * strings) still map to the right translated copy.
 *
 * Exported for test-only validation that each substring still appears somewhere
 * in the Rust backend (see `errors.sync.test.ts`).
 */
export const MESSAGE_KEYS: Record<string, string> = {
  pdfium: "errors.pdfium",
  "cannot open file": "errors.fileNotFound",
  "no such file or directory": "errors.fileNotFound",
  "book file not found": "errors.fileNotFound",
  "permission denied": "errors.permissionDenied",
  "invalid format": "errors.invalidFormat",
  "unsupported file format": "errors.invalidFormat",
  duplicate: "errors.duplicate",
  "chapter index": "errors.chapterIndex",
  corrupt: "errors.corrupt",
  "timed out": "errors.timeout",
  "request timed out": "errors.timeout",
  timeout: "errors.timeout",
  "connection refused": "errors.networkError",
  "connection reset": "errors.networkError",
  "network error": "errors.networkError",
  "http error": "errors.networkError",
  "dns error": "errors.networkError",
  "url blocked": "errors.urlBlocked",
  "too large": "errors.tooLarge",
  "could not find eocd": "errors.notZip",
  "not a valid zip": "errors.notZip",
  "not a valid rar": "errors.notRar",
  "incorrect password": "errors.incorrectPassword",
  "incorrect current password": "errors.incorrectPassword",
  "current password is required": "errors.currentPasswordRequired",
  "profile has no lock to remove": "errors.profileNotLocked",
};

/**
 * Map an error from the backend to a user-facing, translated message.
 *
 * Accepts the raw value thrown by `invoke()` (structured `{kind, message}`,
 * a string, an Error, or anything else). Translation priority:
 *
 *   1. Message-substring match (most specific user-facing copy)
 *   2. Fallback to `kind` → default key (NotFound, PermissionDenied, Network)
 *   3. Raw message (better than a generic "Something went wrong")
 */
/**
 * Detect file-system errors that should trigger the missing-file recovery
 * dialog: book not found, permission denied, locked, or drive ejected.
 *
 * Intentionally excludes generic NotFound (missing TOC entry, missing page)
 * which the backend also raises with that kind.
 */
/**
 * Narrow predicate: only the on-disk book file being absent. Gates the
 * destructive "Remove from library" recovery dialog in Reader.
 */
export function isBookFileMissing(raw: unknown): boolean {
  const { message } = toCarrelError(raw);
  return message.toLowerCase().includes("book file not found");
}

/**
 * Detect the profile soft-lock gate (`CarrelError::LockRequired`, backend
 * `kind: "LockRequired"`). `switch_profile` and the active-profile gate on
 * every data-bearing command raise this when the target profile has a
 * stored lock that hasn't been unlocked this session — the frontend shows
 * the unlock prompt instead of a generic error toast.
 */
export function isLockRequired(raw: unknown): boolean {
  return toCarrelError(raw).kind === "LockRequired";
}

export function isBookFileError(raw: unknown): boolean {
  const { kind, message } = toCarrelError(raw);
  if (kind === "PermissionDenied") return true;
  const lower = message.toLowerCase();
  return (
    lower.includes("book file not found") ||
    lower.includes("permission denied") ||
    lower.includes("file is locked") ||
    lower.includes("resource busy")
  );
}

export function friendlyError(raw: unknown, t: TFunction): string {
  const { kind, message } = toCarrelError(raw);
  const lower = message.toLowerCase();

  for (const [key, translationKey] of Object.entries(MESSAGE_KEYS)) {
    if (lower.includes(key)) return t(translationKey);
  }

  if (kind && KIND_TO_KEY[kind]) {
    return t(KIND_TO_KEY[kind]);
  }

  return message;
}
