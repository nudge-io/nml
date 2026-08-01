// The slice of the `vscode` module surface that loading
// `vscode-languageclient/node`, `@vscode/wasm-wasi/v1`, and the extension's
// own modules touches under the unit harness (plain Node, no extension host).
// The classes exist to be extendable at module load; only the members the
// unit tests actually drive have behavior. Message toasts are recorded so
// tests can assert user-visible reporting.

export class CallHierarchyItem {}
export class CancellationError extends Error {}
export class CodeAction {}
export class CodeLens {}
export class CompletionItem {}
export class Diagnostic {}
export class DocumentLink {}
export class InlayHint {}
export class SymbolInformation {}
export class TypeHierarchyItem {}

export class Uri {
  private constructor(private readonly value: string) {}

  static parse(value: string): Uri {
    return new Uri(value);
  }

  toString(): string {
    return this.value;
  }
}

export interface Disposable {
  dispose(): void;
}

export const shownErrorMessages: string[] = [];
export const shownWarningMessages: string[] = [];

/** Reset recorded toasts between tests. */
export function resetStubRecords(): void {
  shownErrorMessages.length = 0;
  shownWarningMessages.length = 0;
}

export const window = {
  showErrorMessage(message: string): Promise<undefined> {
    shownErrorMessages.push(message);
    return Promise.resolve(undefined);
  },
  showWarningMessage(message: string): Promise<undefined> {
    shownWarningMessages.push(message);
    return Promise.resolve(undefined);
  },
  showInformationMessage(): Promise<undefined> {
    return Promise.resolve(undefined);
  },
};

export const workspace = {
  workspaceFolders: undefined as undefined,
  isTrusted: true,
  getConfiguration(_section?: string): {
    get<T>(key: string, defaultValue: T): T;
  } {
    return {
      get<T>(_key: string, defaultValue: T): T {
        return defaultValue;
      },
    };
  },
  createFileSystemWatcher(_glob: string): Disposable {
    return { dispose(): void {} };
  },
};
