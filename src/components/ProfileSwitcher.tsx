import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { friendlyError, isLockRequired } from "../lib/errors";
import ConfirmDialog from "./ConfirmDialog";
import ProfileUnlockDialog from "./ProfileUnlockDialog";

interface Profile {
  name: string;
  is_active: boolean;
}

interface ProfileSwitcherProps {
  onSwitch: () => void;
}

export default function ProfileSwitcher({ onSwitch }: ProfileSwitcherProps) {
  const { t } = useTranslation();
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [open, setOpen] = useState(false);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [lockOnCreate, setLockOnCreate] = useState(false);
  const [lockPassword, setLockPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [switching, setSwitching] = useState<string | null>(null);
  const [lockPrompt, setLockPrompt] = useState<string | null>(null);
  const dropdownRef = useRef<HTMLDivElement>(null);

  const loadProfiles = useCallback(async () => {
    try {
      const ps = await invoke<Profile[]>("get_profiles");
      setProfiles(ps);
    } catch {
      // non-fatal
    }
  }, []);

  useEffect(() => { loadProfiles(); }, [loadProfiles]);

  // A switch can originate outside this component now — the web UI can move the
  // active profile, and there is one active profile shared by every client — so
  // follow the backend's event instead of assuming this dropdown caused it.
  useEffect(() => {
    const unlisten = listen<string>("profile-changed", () => { loadProfiles(); });
    return () => { unlisten.then((fn) => fn()); };
  }, [loadProfiles]);

  // Close dropdown on outside click
  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false);
        setCreating(false);
        setLockOnCreate(false);
        setLockPassword("");
        setError(null);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  const activeProfile = profiles.find((p) => p.is_active)?.name ?? "default";

  const handleSwitch = async (name: string) => {
    if (name === activeProfile) { setOpen(false); return; }
    if (switching) return; // a switch (with its library re-scan) is in flight
    setSwitching(name);
    try {
      await invoke("switch_profile", { name });
      await loadProfiles();
      setOpen(false);
      onSwitch();
    } catch (err) {
      // A soft-locked profile (A-M3): show the unlock prompt instead of a
      // generic error. `handleUnlocked` retries this same switch once
      // `unlock_profile`/the recovery reset succeeds.
      if (isLockRequired(err)) {
        setLockPrompt(name);
      } else {
        setError(friendlyError(err, t));
      }
    } finally {
      setSwitching(null);
    }
  };

  const handleUnlocked = () => {
    const name = lockPrompt;
    setLockPrompt(null);
    if (name) handleSwitch(name);
  };

  const trimmedLockPassword = lockPassword.trim();
  const createDisabled = !newName.trim() || (lockOnCreate && !trimmedLockPassword);

  const handleCreate = async () => {
    const trimmed = newName.trim();
    if (!trimmed) return;
    if (lockOnCreate && !trimmedLockPassword) return;
    setError(null);
    try {
      await invoke(
        "create_profile",
        lockOnCreate && trimmedLockPassword
          ? { name: trimmed, password: lockPassword }
          : { name: trimmed },
      );
      await invoke("switch_profile", { name: trimmed });
      await loadProfiles();
      setNewName("");
      setLockOnCreate(false);
      setLockPassword("");
      setCreating(false);
      setOpen(false);
      onSwitch();
    } catch (err) {
      setError(friendlyError(err, t));
    }
  };

  const handleDelete = async (name: string) => {
    if (deleting) return; // guard against a double-click firing two deletes
    setError(null);
    setDeleting(true);
    try {
      await invoke("delete_profile", { name });
      setPendingDelete(null);
      await loadProfiles();
    } catch (err) {
      // Keep the dialog open and surface the failure inside it — the
      // dropdown's only error <p> lives in the create-profile branch.
      setError(friendlyError(err, t));
    } finally {
      setDeleting(false);
    }
  };

  // Don't show switcher if only default profile
  if (profiles.length <= 1 && !open) {
    return (
      <button
        onClick={() => { loadProfiles(); setOpen(true); }}
        className="text-xs text-ink-muted hover:text-ink transition-colors ml-2"
        title={t("profiles.manageProfiles")}
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none">
          <circle cx="12" cy="8" r="4" stroke="currentColor" strokeWidth="1.5" />
          <path d="M4 21v-1a6 6 0 0112 0v1" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          <path d="M20 11v4m-2-2h4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      </button>
    );
  }

  return (
    <div className="relative ml-2" ref={dropdownRef}>
      <button
        onClick={() => { setOpen(!open); if (!open) loadProfiles(); }}
        className="flex items-center gap-1.5 px-2 py-1 text-xs text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-lg transition-colors"
      >
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none">
          <circle cx="12" cy="8" r="4" stroke="currentColor" strokeWidth="1.5" />
          <path d="M4 21v-1a6 6 0 0112 0v1" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
        {activeProfile}
        <svg width="10" height="10" viewBox="0 0 20 20" fill="none" className={`transition-transform ${open ? "rotate-180" : ""}`}>
          <path d="M5 7l5 5 5-5" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>

      {open && (
        <div className="absolute top-full left-0 mt-1 w-52 bg-surface border border-warm-border rounded-xl shadow-lg z-50 py-1 animate-fade-in">
          {profiles.map((p) => (
            <div
              key={p.name}
              className={`group flex items-center gap-2 px-3 py-2 transition-colors ${
                switching ? "cursor-wait" : "cursor-pointer"
              } ${
                p.is_active ? "bg-accent-light text-accent" : "text-ink-muted hover:text-ink hover:bg-warm-subtle"
              } ${switching && switching !== p.name ? "opacity-50" : ""}`}
              aria-busy={switching === p.name}
              onClick={() => { if (!switching) handleSwitch(p.name); }}
            >
              <span className="flex-1 text-sm font-medium truncate">{p.name}</span>
              {switching === p.name && (
                <svg
                  className="w-3.5 h-3.5 shrink-0 animate-spin text-accent"
                  viewBox="0 0 24 24"
                  fill="none"
                  role="status"
                  aria-label={t("profiles.switching")}
                >
                  <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="3" className="opacity-25" />
                  <path d="M21 12a9 9 0 0 0-9-9" stroke="currentColor" strokeWidth="4" strokeLinecap="round" />
                </svg>
              )}
              {switching !== p.name && p.is_active && (
                <span className="w-1.5 h-1.5 rounded-full bg-accent shrink-0" />
              )}
              {!switching && !p.is_active && p.name !== "default" && (
                <button
                  onClick={(e) => { e.stopPropagation(); setPendingDelete(p.name); }}
                  className="opacity-0 group-hover:opacity-100 p-0.5 text-ink-muted hover:text-red-500 transition-all"
                  aria-label={t("profiles.deleteLabel", { name: p.name })}
                >
                  <svg width="12" height="12" viewBox="0 0 20 20" fill="none">
                    <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                  </svg>
                </button>
              )}
            </div>
          ))}

          <div className="mx-2 my-1 border-t border-warm-border" />

          {creating ? (
            <div className="px-3 py-2 space-y-2">
              <input
                type="text"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") handleCreate(); if (e.key === "Escape") { setCreating(false); setLockOnCreate(false); setLockPassword(""); setError(null); } }}
                placeholder={t("profiles.profileName")}
                autoFocus
                className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-2.5 py-1.5 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
              />
              <label className="flex items-center gap-1.5 text-xs text-ink-muted cursor-pointer select-none">
                <input
                  type="checkbox"
                  checked={lockOnCreate}
                  onChange={(e) => setLockOnCreate(e.target.checked)}
                  className="rounded border-warm-border"
                />
                {t("profiles.lockThisProfile")}
              </label>
              {lockOnCreate && (
                <div className="space-y-1">
                  <input
                    type="password"
                    value={lockPassword}
                    onChange={(e) => setLockPassword(e.target.value)}
                    onKeyDown={(e) => { if (e.key === "Enter") handleCreate(); if (e.key === "Escape") { setCreating(false); setLockOnCreate(false); setLockPassword(""); setError(null); } }}
                    placeholder={t("profiles.lockPasswordPlaceholder")}
                    aria-label={t("profiles.lockPasswordPlaceholder")}
                    className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-2.5 py-1.5 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                  />
                  <p className="text-[10px] text-ink-muted leading-relaxed px-0.5">
                    {t("settings.profileLockHonestSentence")}
                  </p>
                </div>
              )}
              {error && <p className="text-[10px] text-red-500">{error}</p>}
              <div className="flex gap-1.5">
                <button
                  onClick={handleCreate}
                  disabled={createDisabled}
                  className="flex-1 py-1 text-xs font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors disabled:opacity-40"
                >
                  {t("common.create")}
                </button>
                <button
                  onClick={() => { setCreating(false); setLockOnCreate(false); setLockPassword(""); setError(null); }}
                  className="flex-1 py-1 text-xs text-ink-muted hover:text-ink transition-colors"
                >
                  {t("common.cancel")}
                </button>
              </div>
            </div>
          ) : (
            <button
              onClick={() => setCreating(true)}
              className="w-full px-3 py-2 text-xs text-ink-muted hover:text-accent hover:bg-warm-subtle transition-colors text-left flex items-center gap-1.5"
            >
              <svg width="12" height="12" viewBox="0 0 20 20" fill="none">
                <path d="M10 4v12M4 10h12" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
              </svg>
              {t("profiles.newProfile")}
            </button>
          )}
        </div>
      )}

      {pendingDelete && (
        <ConfirmDialog
          title={t("profiles.deleteConfirmTitle", { name: pendingDelete })}
          message={t("profiles.deleteConfirmMessage")}
          confirmLabel={t("profiles.deleteConfirm")}
          confirmDisabled={deleting}
          onConfirm={() => handleDelete(pendingDelete)}
          onCancel={() => { setPendingDelete(null); setError(null); }}
        >
          {error && <p className="text-xs text-red-500">{error}</p>}
        </ConfirmDialog>
      )}

      {lockPrompt && (
        <ProfileUnlockDialog
          profile={lockPrompt}
          onUnlocked={handleUnlocked}
          onCancel={() => setLockPrompt(null)}
        />
      )}
    </div>
  );
}
