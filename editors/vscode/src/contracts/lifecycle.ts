export type ClientLifecycleState =
  | "absent"
  | "starting"
  | "running"
  | "failed"
  | "disconnected";

/** Numeric mirrors of `vscode-languageclient` `State` — host-free for unit tests. */
export const LspClientState = {
  Stopped: 1,
  Running: 2,
  Starting: 3,
  StartFailed: 4,
} as const;

export type LspClientStateValue = (typeof LspClientState)[keyof typeof LspClientState];
