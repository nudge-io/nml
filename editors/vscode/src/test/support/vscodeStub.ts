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

  /** A real filesystem path as a `file:` URI — enough for the discovery
   *  ladder, which joins a workspace folder with a file name and reads it. */
  static file(fsPath: string): Uri {
    return new Uri(`file://${fsPath}`);
  }

  static joinPath(base: Uri, ...segments: string[]): Uri {
    return new Uri([base.toString().replace(/\/$/, ""), ...segments].join("/"));
  }

  get fsPath(): string {
    return this.value.startsWith("file://") ? this.value.slice("file://".length) : this.value;
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

/** Every modal/non-modal information message shown, with the options and
 *  items it offered — the consent prompt is a security surface, so a test
 *  can assert what the operator was actually told. */
export interface ShownInformationMessage {
  message: string;
  options: unknown;
  items: string[];
}
export const shownInformationMessages: ShownInformationMessage[] = [];

/** Answers `showInformationMessage` hands back, oldest first. An empty queue
 *  answers `undefined` — the operator dismissing the dialog. */
export const informationMessageAnswers: (string | undefined)[] = [];

/** Reset recorded toasts between tests. */
export function resetStubRecords(): void {
  shownErrorMessages.length = 0;
  shownWarningMessages.length = 0;
  shownInformationMessages.length = 0;
  informationMessageAnswers.length = 0;
  informationMessageHook.run = undefined;
  workspace.workspaceFolders = undefined;
  workspace.isTrusted = true;
  workspaceFiles.clear();
  statOverrides.clear();
  configurationValues.clear();
}

/** Files `workspace.fs.readFile` answers, by `Uri.toString()`. */
export const workspaceFiles = new Map<string, string>();

/** `FileType`, as the extension host spells it (LSP-independent, from
 *  `vscode.d.ts`): a bit set, so a symlink to a file is `File | SymbolicLink`
 *  and a FIFO, socket or device is `Unknown` (0). The extension reads it to
 *  refuse a `nml-project.nml` that is not a regular file. */
export const FileType = {
  Unknown: 0,
  File: 1,
  Directory: 2,
  SymbolicLink: 64,
} as const;

/** What `workspace.fs.stat` answers for a path in [`workspaceFiles`], when a
 *  test needs something other than "a regular file of its own length" — a
 *  device or FIFO (`type: FileType.Unknown`), or a file that claims a size
 *  its content does not have. */
export const statOverrides = new Map<string, { type?: number; size?: number }>();

/** Settings `workspace.getConfiguration(section).get(key)` answers, keyed
 *  `"<section>.<key>"`. A key with no entry answers the caller's default,
 *  which is what an unset setting does. Without this the stub could only
 *  ever answer defaults, so the whole `nml.server.path` arm of
 *  `resolveNeutralServer` was unreachable from a unit test. */
export const configurationValues = new Map<string, unknown>();

export const window = {
  showErrorMessage(message: string): Promise<undefined> {
    shownErrorMessages.push(message);
    return Promise.resolve(undefined);
  },
  showWarningMessage(message: string): Promise<undefined> {
    shownWarningMessages.push(message);
    return Promise.resolve(undefined);
  },
  showInformationMessage(
    message: string,
    ...rest: unknown[]
  ): Promise<string | undefined> {
    const [first, ...others] = rest;
    const modal = typeof first === "object" && first !== null;
    shownInformationMessages.push({
      message,
      options: modal ? first : undefined,
      items: (modal ? others : rest).map(String),
    });
    if (informationMessageHook.run) informationMessageHook.run();
    return Promise.resolve(informationMessageAnswers.shift());
  },
};

/** Something that happens WHILE the modal is up.
 *
 *  A modal is human time — seconds to minutes — and the security questions
 *  the extension answers before it are questions about a filesystem that
 *  anyone else on the machine can change in the meantime. A test that wants
 *  to model that needs a hook exactly here, because "during the prompt" is
 *  not a moment any other seam exposes. */
export const informationMessageHook: { run?: () => void } = {};

export const commands = {
  executed: [] as string[],
  executeCommand(command: string): Promise<undefined> {
    commands.executed.push(command);
    return Promise.resolve(undefined);
  },
};

export interface StubWorkspaceFolder {
  readonly name: string;
  readonly uri: Uri;
}

export const workspace = {
  workspaceFolders: undefined as readonly StubWorkspaceFolder[] | undefined,
  isTrusted: true,
  fs: {
    readFile(uri: Uri): Promise<Uint8Array> {
      const text = workspaceFiles.get(uri.toString());
      if (text === undefined) return Promise.reject(new Error(`ENOENT ${uri.toString()}`));
      return Promise.resolve(new TextEncoder().encode(text));
    },
    stat(uri: Uri): Promise<{ type: number; size: number }> {
      const text = workspaceFiles.get(uri.toString());
      if (text === undefined) {
        return Promise.reject(new Error(`ENOENT ${uri.toString()}`));
      }
      const override = statOverrides.get(uri.toString()) ?? {};
      return Promise.resolve({
        type: override.type ?? FileType.File,
        size: override.size ?? new TextEncoder().encode(text).byteLength,
      });
    },
  },
  getConfiguration(section?: string): {
    get<T>(key: string, defaultValue: T): T;
  } {
    return {
      get<T>(key: string, defaultValue: T): T {
        const full = section === undefined ? key : `${section}.${key}`;
        return configurationValues.has(full)
          ? (configurationValues.get(full) as T)
          : defaultValue;
      },
    };
  },
  createFileSystemWatcher(_glob: string): Disposable {
    return { dispose(): void {} };
  },
};
