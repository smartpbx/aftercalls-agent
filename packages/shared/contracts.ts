export type SaveOp =
  | {
      kind: "notes";
      callId: string;
      notes: string;
      at: number;
    }
  | {
      kind: "patchActionItem";
      callId: string;
      itemId: string;
      body: {
        description?: string;
        assignee_user_id?: string | null;
        status?: "open" | "done";
        due_kind?: "none" | "asap" | "dated";
        due_at?: string | null;
      };
      at: number;
    };

export type QueuedSaveEntry = SaveOp & { id: string };

export interface OnlineStatusStore {
  readonly online: boolean;
  start(): void;
  probeNow(): Promise<boolean>;
  subscribe(cb: (online: boolean) => void): () => void;
}

export interface SaveQueueStore {
  readonly items: QueuedSaveEntry[];
  enqueue(op: SaveOp): void;
  flush(): Promise<void>;
}

export interface ZohoStore {
  readonly connected: boolean;
  readonly connectedByDisplayName: string | null;
  readonly loaded: boolean;
  refresh(): Promise<void>;
  ensureCrossTabListener(): void;
}
