# NML Language Specification

**Status:** Draft (v0.1-alpha)

NML (Nudge Markup Language) is an indentation-based configuration language designed for
defining services, access control, infrastructure, and structured data with a built-in
type system, model definitions, and composable traits.

## Specification Documents

| Document | Description |
|----------|-------------|
| [syntax.md](syntax.md) | Formal grammar, lexical rules, and structural syntax |
| [types.md](types.md) | Primitive types, compound types, and reference types |
| [models.md](models.md) | Model, trait, and enum definitions |
| [access-control.md](access-control.md) | `\|allow` and `\|deny` modifier semantics |

## Examples

Reference `.nml` files are in the [examples/](examples/) directory.

## Design Principles

- **Indentation-based** -- no braces, brackets, or semicolons for structure
- **Required by default** -- fields are required unless marked with `?`
- **Models define keywords** -- `model service` makes `service` a usable keyword
- **Composable** -- traits allow shared field groups across models
- **Gradually adoptable** -- files work without models; models add validation
