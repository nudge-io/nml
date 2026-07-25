<!-- Attribution is deliberately generic pre-launch. At the platform's
     public launch: name it, add the logo, link the product — one edit,
     tracked on the launch checklist. -->

# Case study: how a production workflow platform embeds NML

A multi-tenant workflow-automation platform — sandboxed WASM plugins,
per-tenant applications, a fleet of worker processes — uses NML as its
*entire* configuration surface: server config, tenant app definitions,
workflow specifications, and plugin manifests. Its embedding exercises
every layer this repo ships, and several of the library's capabilities
were hardened against this platform's production requirements before any
public release. This page describes the architecture; every API named
links to a cookbook recipe you can run.

## Schema delivery: users' editors always match the running binary

The platform ships its schemas as a content-addressed
[schema package](guides/schema-packages-and-store.md) over three channels:
committed package files in customer projects (zero-config editor
validation), a per-user store its CLI's `schema sync` publishes to, and —
the strongest channel — [an embedded language server](guides/embed-the-lsp.md):
its CLI exposes `<tool> lsp`, so customers' editors validate against the
exact binary they run. Version skew between "what the tool accepts" and
"what the editor says" is structurally impossible; there is nothing to
sync and nothing to get stale. The editor trust model matters at this
scale: the extension only launches the tool's server from PATH, in trusted
workspaces, after a per-workspace prompt — a hostile repository cannot
redirect anyone's editor.

## Zero-downtime reconfiguration: the diff decides, the schema classifies

Operators change live config; restarting a fleet for a rate-limit tweak is
unacceptable, but *pretending* a port change applied is worse. The
platform's reload path is built on the
[semantic diff](guides/diff-and-classify.md): on reload it diffs old and
new config — by meaning, never by text — and classifies every change
through the schema's own `#live`/`#restart`
[directive vocabulary](guides/directive-vocabulary.md). Live changes
hot-swap (egress policy, rate limits, capability grants — swapped via
lock-free pointers with no session interruption); restart-class changes
are *reported truthfully* instead of silently half-applied. The same
declarations drive the operator CLI's `--check` mode and a
kubectl-rollout-style fleet status command, so "what will this change do"
has one answer everywhere. Changes to `secret`-typed fields are flagged by
the diff (`is_secret`) and redacted from every log line.

## Config as a security surface

The platform treats tenant config as untrusted input, and leans on the
library's postures: resilient parsing with bounded diagnostics (a hostile
file cannot flood output — see [collect all errors](guides/collect-all-errors.md)),
reference-only secrets resolved through an injected
[resolver](guides/custom-secret-resolver.md) (credentials cannot appear in
committed tenant files, and the platform's own resolver decides the
source), and format-preserving [structural edits](guides/edit-without-reformatting.md)
whose refuse-don't-guess contract means an ambiguous path or unparseable
snippet can never silently misdirect a write into a tenant's config.

## What it looks like in numbers

The platform's own test suite — several thousand tests — builds against
these exact APIs daily; its parse/validate/diff/reload pipeline runs the
same `nml-core` + `nml-validate` pairing the [footprint page](footprint.md)
measures (19 and 29 packages respectively — the async stack only exists in
its `lsp` subcommand). Multiple capabilities in this repo exist because
this embedding demanded them: semantic-diff fidelity fixes, live-reload
classification (`#live`/`#restart` as schema properties), the schema-package
store, and the provider-tool editor channel all graduated from its
production requirements into the library you can use today.

*The platform is unnamed here until its public launch; the architecture,
APIs, and numbers are real and current.*
