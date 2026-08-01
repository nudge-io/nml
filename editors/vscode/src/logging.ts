import { ExtensionContext, LogOutputChannel, window } from "vscode";

export interface NmlLogs {
  readonly client: LogOutputChannel;
  readonly trace: LogOutputChannel;
  info(message: string): void;
  warn(message: string): void;
  error(message: string): void;
  showClient(): void;
  showTrace(): void;
}

export function createNmlLogs(context: ExtensionContext): NmlLogs {
  const client = window.createOutputChannel("NML Language Server", { log: true });
  const trace = window.createOutputChannel("NML Language Server Trace", { log: true });
  context.subscriptions.push(client, trace);

  return {
    client,
    trace,
    info(message: string): void {
      client.info(message);
    },
    warn(message: string): void {
      client.warn(message);
    },
    error(message: string): void {
      client.error(message);
    },
    showClient(): void {
      client.show();
    },
    showTrace(): void {
      trace.show();
    },
  };
}
