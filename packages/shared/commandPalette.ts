export type CommandPaletteSectionId =
  | "calls"
  | "actions"
  | "pages"
  | "saved-views"
  | "commands";

export type CommandPaletteItemKind =
  | "Call"
  | "Action"
  | "Page"
  | "Command";

export type CommandPaletteItem = {
  id: string;
  section: CommandPaletteSectionId;
  kind: CommandPaletteItemKind;
  title: string;
  subtitle?: string;
  keywords?: string[];
  priority?: number;
  run: () => void | Promise<void>;
};

export type CommandPaletteProvider = (
  query: string,
) => Promise<CommandPaletteItem[]>;

export type CommandPaletteSection = {
  id: CommandPaletteSectionId;
  title: string;
  providerKey?: string;
  items?: CommandPaletteItem[];
  provider?: CommandPaletteProvider;
  loadingLabel?: string;
  errorLabel?: string;
  limit?: number;
};

export type CommandPaletteVisibleItem =
  | { kind: "item"; item: CommandPaletteItem; sectionId: CommandPaletteSectionId }
  | { kind: "loading"; id: string; label: string; sectionId: CommandPaletteSectionId }
  | { kind: "error"; id: string; label: string; sectionId: CommandPaletteSectionId }
  | { kind: "empty"; id: string };

function normalize(input: string): string {
  return input
    .toLocaleLowerCase()
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9]+/g, " ")
    .trim();
}

function itemHaystack(item: CommandPaletteItem): string {
  return normalize(
    [
      item.title,
      item.subtitle ?? "",
      ...(item.keywords ?? []),
    ].join(" "),
  );
}

function scoreItem(item: CommandPaletteItem, query: string): number {
  const q = normalize(query);
  const base = item.priority ?? 0;
  if (!q) return 1000 + base;

  const hay = itemHaystack(item);
  if (!hay) return -1;
  if (hay === q) return 10000 + base;
  if (hay.startsWith(q)) return 9000 - hay.length + base;
  const idx = hay.indexOf(q);
  if (idx >= 0) return 8000 - idx - hay.length * 0.01 + base;

  const tokens = q.split(/\s+/).filter(Boolean);
  if (tokens.length > 1 && tokens.every((token) => hay.includes(token))) {
    return 6500 - hay.length * 0.01 + base;
  }

  let pos = 0;
  let gaps = 0;
  for (const ch of q.replace(/\s+/g, "")) {
    const next = hay.indexOf(ch, pos);
    if (next < 0) return -1;
    gaps += next - pos;
    pos = next + 1;
  }
  return 4500 - gaps - hay.length * 0.01 + base;
}

export function filterCommandPaletteItems(
  items: CommandPaletteItem[],
  query: string,
  limit = 8,
): CommandPaletteItem[] {
  return items
    .map((item, index) => ({ item, index, score: scoreItem(item, query) }))
    .filter((row) => row.score >= 0)
    .sort((a, b) => b.score - a.score || a.index - b.index)
    .slice(0, limit)
    .map((row) => row.item);
}
