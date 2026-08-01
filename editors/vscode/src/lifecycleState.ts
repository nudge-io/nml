import {
  ClientLifecycleState,
  LspClientState,
  LspClientStateValue,
} from "./contracts/lifecycle";

export interface LifecycleInputs {
  connectionLost: boolean;
  hasClient: boolean;
  clientState: LspClientStateValue;
}

/** Pure lifecycle mapping for status presentation — unit-tested without a VS Code host. */
export function resolveLifecycleState(input: LifecycleInputs): ClientLifecycleState {
  if (input.connectionLost) return "disconnected";
  if (!input.hasClient) {
    if (input.clientState === LspClientState.Starting) return "starting";
    if (input.clientState === LspClientState.StartFailed) return "failed";
    return "absent";
  }
  if (input.clientState === LspClientState.Starting) return "starting";
  if (input.clientState === LspClientState.StartFailed) return "failed";
  if (input.clientState === LspClientState.Stopped) return "disconnected";
  return "running";
}
