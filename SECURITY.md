# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately via
[GitHub Security Advisories](https://github.com/nudge-io/nml/security/advisories/new)
("Report a vulnerability"). Do not open a public issue for security reports.

You should receive an acknowledgment within a few business days. Please
include a minimal reproduction where possible.

## Scope notes

Areas of particular interest:

- Parser robustness: inputs that cause panics, non-termination, or
  pathological memory/CPU use in `nml-core` (the parser is used on untrusted
  files in editor contexts).
- Schema-package store integrity: content-hash verification and store
  resolution in `nml-validate`.
- The VS Code extension's server-discovery trust boundary (`<tool> lsp`
  launch gating).
- Extension supply chain: root `pnpm-lock.yaml`, `pnpm-workspace.yaml`
  policy, and `pnpm run check:toolchain` (see `editors/vscode/VSCODE-API.md`).

## Supported versions

Pre-1.0: fixes land on `main` and ship in the next release; there are no
long-term support branches.
