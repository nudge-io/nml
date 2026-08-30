# NML RFCs

Language and platform changes go through an RFC. Process: see
[CONTRIBUTING](../../CONTRIBUTING.md#language-changes-rfcs). An RFC includes a
**Documentation** section and is not *Implemented* until its docs have landed.

**This table is the authoritative status list — keep it and each RFC's
`Status` header in sync.**

| RFC | Title | Status |
|-----|-------|--------|
| [0001](0001-schema-driven-defaulting.md) | Schema-Driven Defaulting | Implemented |
| [0002](0002-visitor-unification-oneof-defaults-workflow-migration.md) | Shared Body-Aware Dispatch, `oneof` Defaults, and Workflow-Subsystem Migration | Implemented |
| [0003](0003-schema-driven-field-completion.md) | Schema-Driven Field Completion (nml-lsp) | Implemented |
| [0004](0004-lossless-cst.md) | Lossless Concrete Syntax Tree (resilient parsing) | Implemented |
| [0005](0005-positional-field-marker.md) | Name-Injection Consistency and the Positional Field Marker (`+`) | Implemented |
| [0006](0006-thin-arrow.md) | The Arrow Token: `->` Replaces `=>` | Implemented |
| [0007](0007-typed-arm-field-types.md) | Typed Arm Field Types: `(K -> V)` | Implemented |
| [0008](0008-unified-diagnostics-error-codes.md) | Unified Core Diagnostics and Stable Error Codes | Implemented |
| [0009](0009-parse-error-taxonomy.md) | Structured Parse Errors and the 0xxx Code Band | Implemented |
| [0010](0010-editor-explanations.md) | In-Editor Error Explanations (Three Tiers) | Tiers 1–2 implemented |
| [0011](0011-traits-non-instantiable-mixins.md) | Traits: Non-Instantiable Mixins | Implemented |
| [0012](0012-one-namespace-self-validating-files.md) | One Namespace: Self-Validating Files & Closed Authority | Implemented |
| [0013](0013-document-scope-deserialization.md) | Document-Scope Deserialization | Implemented |
| [0014](0014-role-conjunction-operator.md) | Role-Conjunction Operator (`&`) | Implemented |
| [0015](0015-nominal-union-annotations.md) | Nominal Union Annotations (`as <Variant>`) | Implemented |
| [0016](0016-exact-decimal-numbers.md) | Exact Decimal Numbers | Implemented |
| [0017](0017-duration-literals.md) | Duration Literals (`30s` as a value) | Implemented |
| [0018](0018-numeric-schema-facets.md) | Numeric Schema Facets (`number(min = 1)`) | Implemented |
| [0019](0019-instance-layers-and-sealed-fields.md) | Instance Composition (`uses`) and Sealed Fields | Slice 1 implemented |
| [0020](0020-instance-imports-and-export-visibility.md) | Instance Imports, Exports, and File Scope | Proposed |
| [0021](0021-string-schema-facets.md) | String Schema Facets (`string(pattern = "…")`) | Proposed |
| [0022](0022-type-aliases.md) | Type Aliases (`type name <type-expr>`) | Proposed |
| [0023](0023-fix-resolution-and-composition-conformance-closure.md) | Structural Fix Resolution and Composition Conformance Closure | Implemented |
| [0024](0024-param-ref-primitive.md) | ParamRef Primitive (`input memberId`) | Proposed |
| [0025](0025-normalize-on-merge.md) | Normalize-on-Merge: Deciding at Merge Time and Deleting the Composition Plan | Proposed |

Source comments also cite consumer-project RFC numbers (e.g. 0019, 0030,
0032, 0035 — the nudge series) as design tags. A number with a row above
is a document in this directory; anything else is a consumer tag.
