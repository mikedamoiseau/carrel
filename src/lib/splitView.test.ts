import { beforeEach, describe, expect, it } from "vitest";
import {
  applySwap,
  canPersistCompanion,
  effectiveCompanionId,
  readSplitState,
  splitCompanionKey,
  splitModeKey,
  writeCompanion,
  writeSplitMode,
} from "./splitView";

function makeStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k) => (map.has(k) ? map.get(k)! : null),
    key: (i) => Array.from(map.keys())[i] ?? null,
    removeItem: (k) => {
      map.delete(k);
    },
    setItem: (k, v) => {
      map.set(k, v);
    },
  };
}

describe("split-view storage keys", () => {
  it("derives stable per-book keys", () => {
    expect(splitModeKey("abc")).toBe("carrel-split-mode-abc");
    expect(splitCompanionKey("abc")).toBe("carrel-split-companion-abc");
  });
});

describe("readSplitState", () => {
  let storage: Storage;
  beforeEach(() => {
    storage = makeStorage();
  });

  it("returns split off / companion null on empty storage", () => {
    expect(readSplitState(storage, "A")).toEqual({
      splitMode: false,
      companionBookId: null,
    });
  });

  it("reads splitMode true only when value is exactly '1'", () => {
    storage.setItem(splitModeKey("A"), "1");
    expect(readSplitState(storage, "A").splitMode).toBe(true);

    storage.setItem(splitModeKey("A"), "true");
    expect(readSplitState(storage, "A").splitMode).toBe(false);
  });

  it("reads companionBookId verbatim", () => {
    storage.setItem(splitCompanionKey("A"), "B");
    expect(readSplitState(storage, "A").companionBookId).toBe("B");
  });
});

describe("writeSplitMode", () => {
  it("sets '1' when on and removes the key when off", () => {
    const storage = makeStorage();

    writeSplitMode(storage, "A", true);
    expect(storage.getItem(splitModeKey("A"))).toBe("1");

    writeSplitMode(storage, "A", false);
    expect(storage.getItem(splitModeKey("A"))).toBeNull();
  });
});

describe("writeCompanion", () => {
  it("writes the companion id and clears with null", () => {
    const storage = makeStorage();

    writeCompanion(storage, "A", "B");
    expect(storage.getItem(splitCompanionKey("A"))).toBe("B");

    writeCompanion(storage, "A", null);
    expect(storage.getItem(splitCompanionKey("A"))).toBeNull();
  });
});

describe("applySwap", () => {
  it("seeds the new primary with old primary as companion", () => {
    const storage = makeStorage();
    storage.setItem(splitModeKey("A"), "1");
    storage.setItem(splitCompanionKey("A"), "B");

    applySwap(storage, "A", "B");

    expect(storage.getItem(splitModeKey("B"))).toBe("1");
    expect(storage.getItem(splitCompanionKey("B"))).toBe("A");
  });

  it("leaves the old primary's pairing intact so navigating back restores the split", () => {
    const storage = makeStorage();
    storage.setItem(splitModeKey("A"), "1");
    storage.setItem(splitCompanionKey("A"), "B");

    applySwap(storage, "A", "B");

    expect(storage.getItem(splitModeKey("A"))).toBe("1");
    expect(storage.getItem(splitCompanionKey("A"))).toBe("B");
  });

  it("survives the round-trip — swap back from B preserves the symmetry", () => {
    const storage = makeStorage();
    storage.setItem(splitModeKey("A"), "1");
    storage.setItem(splitCompanionKey("A"), "B");

    applySwap(storage, "A", "B");
    applySwap(storage, "B", "A");

    expect(readSplitState(storage, "A")).toEqual({
      splitMode: true,
      companionBookId: "B",
    });
    expect(readSplitState(storage, "B")).toEqual({
      splitMode: true,
      companionBookId: "A",
    });
  });
});

describe("effectiveCompanionId", () => {
  it("returns the companion id when set", () => {
    expect(effectiveCompanionId("B", "A")).toBe("B");
  });

  it("falls back to the primary book id when companion is null", () => {
    expect(effectiveCompanionId(null, "A")).toBe("A");
  });
});

describe("canPersistCompanion", () => {
  it("is false when no companion is selected (both panes show the same book)", () => {
    expect(canPersistCompanion(null, "A")).toBe(false);
  });

  it("is false when the companion id equals the primary id", () => {
    expect(canPersistCompanion("A", "A")).toBe(false);
  });

  it("is true when the companion differs from the primary", () => {
    expect(canPersistCompanion("B", "A")).toBe(true);
  });
});
