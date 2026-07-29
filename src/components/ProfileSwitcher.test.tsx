// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, string>) =>
      opts?.name ? `${key}:${opts.name}` : key,
  }),
}));
vi.mock("../lib/useFocusTrap", () => ({ useFocusTrap: () => ({ current: null }) }));
// Captures the "profile-changed" handler so a test can fire the event the Rust
// side emits on every switch, local or remote.
const eventHandlers: Record<string, (e: { payload: unknown }) => void> = {};
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (e: { payload: unknown }) => void) => {
    eventHandlers[name] = handler;
    return Promise.resolve(() => { delete eventHandlers[name]; });
  },
}));

import { render, screen, cleanup, fireEvent, waitFor, act } from "@testing-library/react";
import ProfileSwitcher from "./ProfileSwitcher";

beforeEach(() => {
  invoke.mockReset();
  invoke.mockImplementation((cmd: string) => {
    if (cmd === "get_profiles")
      return Promise.resolve([
        { name: "default", is_active: true },
        { name: "work", is_active: false },
      ]);
    return Promise.resolve(undefined);
  });
});
afterEach(() => cleanup());

async function openDropdown() {
  render(<ProfileSwitcher onSwitch={() => {}} />);
  // wait for profiles to load, then open
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_profiles"));
  await act(async () => {
    fireEvent.click(screen.getByRole("button"));
  });
}

describe("ProfileSwitcher delete confirmation", () => {
  it("does NOT delete immediately when the delete button is clicked — it asks first", async () => {
    await openDropdown();
    const delBtn = await screen.findByLabelText("profiles.deleteLabel:work");
    await act(async () => fireEvent.click(delBtn));

    // No delete_profile call yet — a confirm dialog must intervene.
    expect(invoke).not.toHaveBeenCalledWith("delete_profile", expect.anything());
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("deletes only after the confirm dialog is accepted", async () => {
    await openDropdown();
    const delBtn = await screen.findByLabelText("profiles.deleteLabel:work");
    await act(async () => fireEvent.click(delBtn));

    const confirmBtn = screen.getByRole("button", { name: "profiles.deleteConfirm" });
    await act(async () => fireEvent.click(confirmBtn));

    expect(invoke).toHaveBeenCalledWith("delete_profile", { name: "work" });
  });

  it("does not render a delete control for the active profile", async () => {
    await openDropdown();
    // active profile is "default"; its delete label must be absent
    expect(screen.queryByLabelText("profiles.deleteLabel:default")).not.toBeInTheDocument();
  });
});

describe("ProfileSwitcher switching loading state", () => {
  it("shows a loading spinner on the row while switch_profile is in flight", async () => {
    // Hold switch_profile pending so the loading state stays visible.
    let resolveSwitch: () => void = () => {};
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_profiles")
        return Promise.resolve([
          { name: "default", is_active: true },
          { name: "work", is_active: false },
        ]);
      if (cmd === "switch_profile")
        return new Promise<void>((res) => {
          resolveSwitch = res;
        });
      return Promise.resolve(undefined);
    });

    await openDropdown();
    const workRow = screen.getByText("work").closest("div")!;
    await act(async () => fireEvent.click(workRow));

    // Spinner (role=status) appears while the switch is pending.
    const spinner = await screen.findByRole("status");
    expect(spinner).toBeInTheDocument();
    expect(spinner).toHaveAttribute("aria-label", "profiles.switching");

    // Resolve the switch; spinner goes away.
    await act(async () => {
      resolveSwitch();
    });
    await waitFor(() => expect(screen.queryByRole("status")).not.toBeInTheDocument());
  });
});

describe("ProfileSwitcher create-with-lock", () => {
  async function openCreateForm() {
    await openDropdown();
    const newProfileBtn = await screen.findByText("profiles.newProfile");
    await act(async () => fireEvent.click(newProfileBtn));
  }

  it("does not show a password field until the lock checkbox is checked", async () => {
    await openCreateForm();
    expect(screen.queryByPlaceholderText("profiles.lockPasswordPlaceholder")).not.toBeInTheDocument();

    fireEvent.click(screen.getByText("profiles.lockThisProfile"));
    expect(await screen.findByPlaceholderText("profiles.lockPasswordPlaceholder")).toBeInTheDocument();
  });

  it("passes the password to create_profile when the lock checkbox is checked", async () => {
    await openCreateForm();
    fireEvent.change(screen.getByPlaceholderText("profiles.profileName"), {
      target: { value: "work" },
    });
    fireEvent.click(screen.getByText("profiles.lockThisProfile"));
    fireEvent.change(screen.getByPlaceholderText("profiles.lockPasswordPlaceholder"), {
      target: { value: "hunter2" },
    });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });

    expect(invoke).toHaveBeenCalledWith("create_profile", { name: "work", password: "hunter2" });
  });

  it("omits the password from create_profile when the lock checkbox is unchecked", async () => {
    await openCreateForm();
    fireEvent.change(screen.getByPlaceholderText("profiles.profileName"), {
      target: { value: "work" },
    });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });

    expect(invoke).toHaveBeenCalledWith("create_profile", { name: "work" });
  });

  it("disables the create button when the lock checkbox is checked but the password is blank", async () => {
    await openCreateForm();
    fireEvent.change(screen.getByPlaceholderText("profiles.profileName"), {
      target: { value: "work" },
    });
    fireEvent.click(screen.getByText("profiles.lockThisProfile"));

    expect(screen.getByRole("button", { name: "common.create" })).toBeDisabled();
  });
});

describe("ProfileSwitcher soft-lock handling", () => {
  it("shows the unlock prompt (not a generic error) when switch_profile reports LockRequired", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_profiles")
        return Promise.resolve([
          { name: "default", is_active: true },
          { name: "work", is_active: false },
        ]);
      if (cmd === "switch_profile")
        return Promise.reject({ kind: "LockRequired", message: "Profile 'work' is locked" });
      return Promise.resolve(undefined);
    });

    await openDropdown();
    const workRow = screen.getByText("work").closest("div")!;
    await act(async () => fireEvent.click(workRow));

    expect(await screen.findByRole("dialog", { name: "profiles.unlockTitle:work" })).toBeInTheDocument();
  });

  it("retries the switch after a successful unlock", async () => {
    let switchAttempts = 0;
    invoke.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "get_profiles")
        return Promise.resolve([
          { name: "default", is_active: true },
          { name: "work", is_active: false },
        ]);
      if (cmd === "switch_profile") {
        switchAttempts += 1;
        if (switchAttempts === 1) {
          return Promise.reject({ kind: "LockRequired", message: "Profile 'work' is locked" });
        }
        return Promise.resolve(undefined);
      }
      if (cmd === "unlock_profile") return Promise.resolve(undefined);
      void args;
      return Promise.resolve(undefined);
    });

    await openDropdown();
    const workRow = screen.getByText("work").closest("div")!;
    await act(async () => fireEvent.click(workRow));
    await screen.findByRole("dialog", { name: "profiles.unlockTitle:work" });

    fireEvent.change(screen.getByLabelText("profiles.unlockPasswordLabel"), {
      target: { value: "hunter2" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /profiles.unlock/ }));
    });

    await waitFor(() => expect(switchAttempts).toBe(2));
    expect(invoke).toHaveBeenCalledWith("switch_profile", { name: "work" });
  });
});

// A profile switch can now originate anywhere — another browser tab, a phone, or
// the desktop itself — because one active profile is shared by every client. The
// switcher must follow the backend's `profile-changed` event, or it keeps
// labelling the profile the app started in.
describe("ProfileSwitcher remote switch", () => {
  it("reloads the profile list when the active profile changes elsewhere", async () => {
    render(<ProfileSwitcher onSwitch={() => {}} />);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_profiles"));
    expect(await screen.findByText("default")).toBeInTheDocument();

    // A remote switch lands: the backend now reports `work` as active.
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_profiles")
        return Promise.resolve([
          { name: "default", is_active: false },
          { name: "work", is_active: true },
        ]);
      return Promise.resolve(undefined);
    });

    await act(async () => {
      eventHandlers["profile-changed"]?.({ payload: "work" });
    });

    await waitFor(() => expect(screen.getByText("work")).toBeInTheDocument());
  });
});
