import { invoke } from "@tauri-apps/api/core";
import type { CommandPaletteSection } from "@aftercalls/shared/commandPalette";

type Navigate = (href: string) => void | Promise<void>;
type OpenExternal = (href: string) => void | Promise<void>;

export type AgentCommandPaletteMe = {
  id?: string;
  user_id?: string;
  role: string;
  is_platform_staff?: boolean;
  features?: { zoho?: boolean };
};

type CallListItem = {
  id: string;
  title: string | null;
  matched_client: string | null;
  status: string;
  user_display_name?: string;
  notes?: string;
  tags?: Array<{ kind: string; value: string }>;
};

type CallsListResponse = {
  calls: CallListItem[];
};

type MeActionItem = {
  id: string;
  call_id: string;
  description: string;
  call_title: string | null;
  due_at: string | null;
};

type MeActionItemsResponse = {
  items: MeActionItem[];
};

function callTitle(call: CallListItem): string {
  return call.title?.trim() || "Untitled call";
}

function callSubtitle(call: CallListItem): string {
  return [call.matched_client, call.user_display_name, call.status]
    .filter((v): v is string => !!v)
    .join(" · ");
}

function pageItem(
  id: string,
  title: string,
  href: string,
  run: () => void | Promise<void>,
  keywords: string[] = [],
) {
  return {
    id,
    section: "pages" as const,
    kind: "Page" as const,
    title,
    subtitle: href,
    keywords,
    run,
  };
}

export function createAgentCommandPaletteSections(
  me: AgentCommandPaletteMe | null,
  navigate: Navigate,
  openExternal: OpenExternal,
  startRecording: () => void | Promise<void>,
): CommandPaletteSection[] {
  if (!me) return [];
  const providerUserKey = me.user_id ?? me.id ?? "unknown";
  const isAdmin =
    me.role === "admin" ||
    me.role === "owner" ||
    (me.role as string) === "superadmin";
  const isStaff = me.is_platform_staff === true || (me.role as string) === "superadmin";

  const pages = [
    pageItem("record", "Record", "/", () => navigate("/")),
    pageItem("calls", "Calls", "/calls", () => navigate("/calls")),
    pageItem("actions", "Action items", "/actions", () => navigate("/actions"), ["todos", "tasks"]),
    pageItem("settings", "Settings", "/settings", () => navigate("/settings"), ["account"]),
  ];
  if (isAdmin) {
    pages.push(
      pageItem("team", "Team", "/admin/users", () => openExternal("/admin/users"), ["users"]),
      pageItem("org", "Org", "/admin", () => openExternal("/admin"), ["organization"]),
      pageItem("vocab", "Vocab", "/admin/vocab", () => openExternal("/admin/vocab"), ["vocabulary"]),
    );
    if (me.features?.zoho) {
      pages.push(pageItem("crm", "CRM", "/admin/zoho", () => openExternal("/admin/zoho")));
    }
  }
  if (isStaff) {
    pages.push(
      pageItem("terms", "Terms", "/staff/tos", () => openExternal("/staff/tos")),
      pageItem("logs", "Logs", "/staff/agent-logs", () => openExternal("/staff/agent-logs"), ["agent logs"]),
    );
  }

  return [
    {
      id: "calls",
      title: "Calls",
      providerKey: `agent:${providerUserKey}:calls:${isAdmin ? "all" : "mine"}`,
      loadingLabel: "Loading recent calls...",
      errorLabel: "Calls could not load",
      limit: 8,
      provider: async (query) => {
        const resp = await invoke<CallsListResponse>("list_calls", {
          scope: isAdmin ? "all" : "mine",
          user: null,
          tags: [],
          fromDate: null,
          toDate: null,
          cursor: null,
          limit: 25,
          q: query,
          view: "active",
        });
        return resp.calls.map((call) => ({
          id: call.id,
          section: "calls",
          kind: "Call",
          title: callTitle(call),
          subtitle: callSubtitle(call),
          keywords: [
            call.matched_client ?? "",
            call.user_display_name ?? "",
            call.notes ?? "",
            ...(call.tags ?? []).map((tag) => `${tag.kind} ${tag.value}`),
          ],
          run: () => navigate(`/calls/${call.id}`),
        }));
      },
    },
    {
      id: "actions",
      title: "Action items",
      providerKey: `agent:${providerUserKey}:actions:mine`,
      loadingLabel: "Loading action items...",
      errorLabel: "Action items could not load",
      limit: 8,
      provider: async () => {
        const resp = await invoke<MeActionItemsResponse>("list_me_action_items", {
          status: "open",
          cursor: null,
          limit: 25,
          due: "all",
        });
        return resp.items.map((item) => ({
          id: item.id,
          section: "actions",
          kind: "Action",
          title: item.description,
          subtitle: [item.call_title || "Untitled call", item.due_at ? "Has due date" : ""]
            .filter(Boolean)
            .join(" · "),
          keywords: [item.call_title ?? ""],
          run: () => navigate(`/calls/${item.call_id}`),
        }));
      },
    },
    {
      id: "pages",
      title: "Pages",
      items: pages,
      limit: 12,
    },
    {
      id: "commands",
      title: "Commands",
      items: [
        {
          id: "start-recording",
          section: "commands",
          kind: "Command",
          title: "Start recording",
          subtitle: "Record",
          keywords: ["capture", "call"],
          priority: 100,
          run: startRecording,
        },
      ],
      limit: 4,
    },
  ];
}
