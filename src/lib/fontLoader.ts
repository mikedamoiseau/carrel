const loaded = new Set<string>();

const FONT_MODULES: Record<string, () => Promise<unknown[]>> = {
  serif: () =>
    Promise.all([
      import("@fontsource-variable/lora"),
      import("@fontsource-variable/lora/wght-italic.css"),
    ]),
  literata: () =>
    Promise.all([
      import("@fontsource-variable/literata"),
      import("@fontsource-variable/literata/wght-italic.css"),
    ]),
};

export async function loadFont(key: string): Promise<void> {
  if (loaded.has(key)) return;
  const loader = FONT_MODULES[key];
  if (!loader) return;
  await loader();
  loaded.add(key);
}

export function preloadStoredFont(): void {
  const key = localStorage.getItem("carrel-font-family") ?? "serif";
  if (FONT_MODULES[key]) {
    loadFont(key);
  }
}
