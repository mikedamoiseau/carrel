import { describe, it, expect } from "vitest";
import type { TFunction } from "i18next";
import { friendlyError, toCarrelError, isBookFileError, isBookFileMissing, isLockRequired, isOpdsAuthError } from "./errors";

const mockT = ((key: string) => key) as TFunction;

describe("friendlyError", () => {
    it("maps 'cannot open file' errors", () => {
        expect(friendlyError("Cannot open file: No such file or directory (os error 2)", mockT)).toBe("errors.fileNotFound");
    });

    it("maps 'no such file or directory' errors", () => {
        expect(friendlyError("No such file or directory", mockT)).toBe("errors.fileNotFound");
    });

    it("maps 'book file not found' errors", () => {
        expect(friendlyError("Book file not found at '/path/to/book.epub'", mockT)).toBe("errors.fileNotFound");
    });

    it("maps pdfium errors to PDF-specific message", () => {
        expect(friendlyError("pdfium library not found: some details", mockT)).toBe("errors.pdfium");
    });

    it("does not map pdfium errors to file-not-found", () => {
        expect(friendlyError("pdfium library not found", mockT)).not.toBe("errors.fileNotFound");
    });

    it("maps duplicate errors", () => {
        expect(friendlyError("Book is a duplicate", mockT)).toBe("errors.duplicate");
    });

    it("returns raw error for unknown errors", () => {
        expect(friendlyError("something unknown", mockT)).toBe("something unknown");
    });

    it("maps timeout errors", () => {
        expect(friendlyError("request timed out", mockT)).toBe("errors.timeout");
    });

    it("maps connection refused errors", () => {
        expect(friendlyError("connection refused", mockT)).toBe("errors.networkError");
    });

    it("maps HTTP error messages", () => {
        expect(friendlyError("HTTP error: connection reset", mockT)).toBe("errors.networkError");
    });

    it("maps URL blocked errors", () => {
        expect(friendlyError("URL blocked: only public HTTP/HTTPS URLs are allowed.", mockT)).toBe("errors.urlBlocked");
    });

    it("maps too large errors", () => {
        expect(friendlyError("Response too large (limit: 5 MB).", mockT)).toBe("errors.tooLarge");
    });

    describe("structured CarrelError payloads", () => {
        it("unwraps {kind, message} before matching", () => {
            const err = { kind: "NotFound", message: "Book file not found at '/x.epub'" };
            expect(friendlyError(err, mockT)).toBe("errors.fileNotFound");
        });

        it("falls back to kind when message has no substring match", () => {
            const err = { kind: "NotFound", message: "Entry missing" };
            expect(friendlyError(err, mockT)).toBe("errors.fileNotFound");
        });

        it("maps PermissionDenied kind even with a custom message", () => {
            const err = { kind: "PermissionDenied", message: "Keychain access refused" };
            expect(friendlyError(err, mockT)).toBe("errors.permissionDenied");
        });

        it("maps Network kind to generic network key", () => {
            const err = { kind: "Network", message: "BnF HTTP 503" };
            expect(friendlyError(err, mockT)).toBe("errors.networkError");
        });

        it("falls back to InvalidInput kind when message has no specific match", () => {
            const err = { kind: "InvalidInput", message: "Title cannot be empty." };
            expect(friendlyError(err, mockT)).toBe("errors.invalidInput");
        });

        it("maps Database kind to generic database key", () => {
            const err = { kind: "Database", message: "database is locked" };
            expect(friendlyError(err, mockT)).toBe("errors.database");
        });

        it("maps Io kind to generic io key", () => {
            const err = { kind: "Io", message: "disk full" };
            expect(friendlyError(err, mockT)).toBe("errors.io");
        });

        it("maps Serialization kind to corrupt key", () => {
            const err = { kind: "Serialization", message: "unexpected end of file" };
            expect(friendlyError(err, mockT)).toBe("errors.corrupt");
        });

        it("maps Internal kind to generic key", () => {
            const err = { kind: "Internal", message: "oops" };
            expect(friendlyError(err, mockT)).toBe("errors.generic");
        });

        it("maps LockRequired kind to the lock-required key", () => {
            const err = { kind: "LockRequired", message: "Profile 'work' is locked" };
            expect(friendlyError(err, mockT)).toBe("errors.lockRequired");
        });

        it("maps 'incorrect password' messages regardless of kind", () => {
            const err = { kind: "InvalidInput", message: "Incorrect password" };
            expect(friendlyError(err, mockT)).toBe("errors.incorrectPassword");
        });

        it("maps 'current password is required' messages", () => {
            const err = {
                kind: "InvalidInput",
                message: "Current password is required to change the profile lock",
            };
            expect(friendlyError(err, mockT)).toBe("errors.currentPasswordRequired");
        });

        it("handles Error instances", () => {
            expect(friendlyError(new Error("HTTP error: connection reset"), mockT)).toBe("errors.networkError");
        });

        it("toCarrelError extracts kind+message from payload", () => {
            expect(toCarrelError({ kind: "Internal", message: "oops" })).toEqual({
                kind: "Internal",
                message: "oops",
            });
        });

        it("toCarrelError wraps bare strings", () => {
            expect(toCarrelError("hello")).toEqual({ message: "hello" });
        });

        it("toCarrelError handles null and undefined gracefully", () => {
            expect(toCarrelError(null).message).toBeTruthy();
            expect(toCarrelError(undefined).message).toBeTruthy();
        });
    });
});

describe("isBookFileError", () => {
    it("matches 'book file not found' string errors", () => {
        expect(isBookFileError("Book file not found at '/path/to/book.epub'")).toBe(true);
    });

    it("matches structured NotFound with 'book file not found' message", () => {
        expect(isBookFileError({ kind: "NotFound", message: "Book file not found at '/x.epub'" })).toBe(true);
    });

    it("matches structured PermissionDenied errors", () => {
        expect(isBookFileError({ kind: "PermissionDenied", message: "Access denied to /path/to/book.epub" })).toBe(true);
    });

    it("matches 'permission denied' in message string", () => {
        expect(isBookFileError("permission denied: /path/to/book.epub")).toBe(true);
    });

    it("matches locked file errors", () => {
        expect(isBookFileError("file is locked: /path/to/book.epub")).toBe(true);
    });

    it("matches 'resource busy' errors (drive ejected mid-read)", () => {
        expect(isBookFileError("resource busy: /Volumes/USB/book.epub")).toBe(true);
    });

    it("does NOT match generic NotFound errors (missing TOC entry, etc.)", () => {
        expect(isBookFileError({ kind: "NotFound", message: "Entry missing in archive" })).toBe(false);
    });

    it("does NOT match unrelated errors", () => {
        expect(isBookFileError("invalid format")).toBe(false);
        expect(isBookFileError({ kind: "Network", message: "timeout" })).toBe(false);
    });

    it("handles Error instances", () => {
        expect(isBookFileError(new Error("Book file not found at '/x.epub'"))).toBe(true);
    });

    it("handles null and undefined gracefully", () => {
        expect(isBookFileError(null)).toBe(false);
        expect(isBookFileError(undefined)).toBe(false);
    });
});

describe("isBookFileMissing", () => {
    it("matches 'book file not found' string errors", () => {
        expect(isBookFileMissing("Book file not found at '/path/to/book.epub'")).toBe(true);
    });

    it("matches structured error with 'book file not found' message", () => {
        expect(isBookFileMissing({ kind: "NotFound", message: "Book file not found at '/x.epub'" })).toBe(true);
    });

    it("does NOT match permission denied (file still exists)", () => {
        expect(isBookFileMissing({ kind: "PermissionDenied", message: "Access denied" })).toBe(false);
        expect(isBookFileMissing("permission denied: /path/to/book.epub")).toBe(false);
    });

    it("does NOT match locked file (file still exists)", () => {
        expect(isBookFileMissing("file is locked: /path/to/book.epub")).toBe(false);
    });

    it("does NOT match resource busy (file still exists)", () => {
        expect(isBookFileMissing("resource busy: /Volumes/USB/book.epub")).toBe(false);
    });

    it("does NOT match generic NotFound (missing TOC entry)", () => {
        expect(isBookFileMissing({ kind: "NotFound", message: "Entry missing in archive" })).toBe(false);
    });
});

describe("isOpdsAuthError", () => {
    it("recognizes the backend's auth-required error in every wire shape", () => {
        expect(isOpdsAuthError("OPDS auth required: HTTP 401 Unauthorized")).toBe(true);
        expect(isOpdsAuthError(new Error("OPDS auth required: HTTP 403 Forbidden"))).toBe(true);
        expect(isOpdsAuthError({ kind: "PermissionDenied", message: "OPDS auth required: HTTP 401" })).toBe(true);
        expect(isOpdsAuthError("HTTP error: connection reset")).toBe(false);
        expect(isOpdsAuthError(null)).toBe(false);
    });

    it("maps the auth-required message to its own copy", () => {
        expect(friendlyError("OPDS auth required: HTTP 401", mockT)).toBe("errors.opdsAuthRequired");
    });
});

describe("isLockRequired", () => {
    it("matches the LockRequired kind", () => {
        expect(isLockRequired({ kind: "LockRequired", message: "Profile 'work' is locked" })).toBe(true);
    });

    it("does NOT match other kinds, even with a similar message", () => {
        expect(isLockRequired({ kind: "InvalidInput", message: "Profile 'work' is locked" })).toBe(false);
    });

    it("does NOT match plain strings or Error instances", () => {
        expect(isLockRequired("Profile 'work' is locked")).toBe(false);
        expect(isLockRequired(new Error("locked"))).toBe(false);
    });

    it("handles null and undefined gracefully", () => {
        expect(isLockRequired(null)).toBe(false);
        expect(isLockRequired(undefined)).toBe(false);
    });
});
