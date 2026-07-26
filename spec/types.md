# NML Type System

## Primitive Types

NML has 7 primitive types. Each has a distinct purpose with no overlap.

### `string`

Quoted text. Used for labels, descriptions, addresses, and general text values.

```
label = "Admin Role"
address = "localhost:8001"
```

### `number`

General-purpose **exact decimal** type covering both whole numbers and
decimals with a single surface type.

```
port = 8000
weight = 0.75
taxRate = 0.20
```

There is no separate `int` or `float` type — and no binary floating point
anywhere in the data model. The int/float distinction is an implementation
detail that does not belong in a configuration language.

**Precision guarantee (RFC 0016):** every `number` is stored exactly. The
domain is the finite IEEE 754-2019 decimal128 value space: up to **34
significant digits**, with magnitudes from 10^-6176 up to
9.999…×10^6144. A literal
whose value fits parses bit-for-bit — `taxRate = 0.20` *is* 0.20, not
0.2000000000000000111…, and the written scale is preserved (`2.50` stays
`2.50` through formatting and serialization). A value that cannot be
stored exactly is a parse error (`NML0014`), never a silently rounded
approximation. Trailing zeros beyond the 34-digit budget drop losslessly;
integers well past `u64` (up to 34 digits) are first-class.

Conversion to binary floats happens only at the consumer's edge (an `f64`
struct field, `to_f64()`), correctly rounded, by explicit request.

### `money`

Pairs an exact decimal value with an ISO 4217 currency code. The literal format
is `amount CURRENCY_CODE`:

```
monthlyPrice = 19.99 USD
ukPrice = 6.55 GBP
japanPrice = 1299 JPY
```

#### Internal Representation

Stored as integer minor units using the currency's ISO 4217 exponent:

| Literal | Amount (minor units) | Currency | Exponent |
|---------|---------------------|----------|----------|
| `19.99 USD` | 1999 | USD | 2 |
| `6.55 GBP` | 655 | GBP | 2 |
| `1299 JPY` | 1299 | JPY | 0 |
| `5.125 BHD` | 5125 | BHD | 3 |

This eliminates floating-point precision issues. Invalid precision is a parse
error: `19.999 USD` is rejected because USD has exponent 2.

#### Currency Constraints

Models can restrict which currencies are accepted:

```
price money <currency = "USD">
globalPrice money <currency = ["USD", "GBP", "EUR"]>
```

### `bool`

Unquoted boolean values:

```
enabled = true
debug = false
```

### `duration`

Quoted time duration strings — an unsigned integer immediately followed
by one unit suffix (see [syntax.md](syntax.md) §Duration Literals for the
exact grammar; enforced as `NML2029`):

```
sessionDuration = "72h"
timeout = "30s"
pollInterval = "500ms"
```

Supported units:
- `h` -- hours
- `m` -- minutes
- `s` -- seconds
- `ms` -- milliseconds

### `path`

Quoted URL path strings with support for named variables `{name}` and
wildcards `{*}`:

```
homePath = "/"
userProfile = "/user/{username}"
assets = "/static/{*}"
adminRoute = "/{org}/admin/{dept}/update"
```

### `secret`

Values resolved from environment variables or a secret vault at runtime.
Uses the `$ENV.` prefix:

```
serverToken = $ENV.POSTMARK_SERVER_TOKEN
apiKey = $ENV.API_KEY
```

Secret values are masked in logs and diagnostic output.

## Template Strings

Any string value may contain **template expressions** using `{{namespace.key}}`
syntax. When a string contains template expressions, it is stored as a
`TemplateString` in the AST rather than a plain `String`:

```
greeting = "Hello, {{args.name}}!"
system = "You are {{config.persona}}. Your job is {{config.role}}."
```

Template strings are composed of alternating literal text and expression segments.
The host application resolves expressions at runtime using a namespace-aware
lookup.

Template expressions are distinct from path variables (`{name}`, `{*}`), which
use single braces.

## Fallback Values

Any value may have a **fallback chain** using the `|` operator. Fallbacks provide
default values when the primary value cannot be resolved:

```
apiKey = $ENV.API_KEY | $ENV.FALLBACK_KEY | "dev-default"
port = $ENV.PORT | 3000
```

Fallbacks are evaluated left-to-right. The chain terminates at the first
successfully resolved value. If all values fail to resolve, an error is raised.

Fallback chains are represented as nested `Fallback(primary, fallback)` nodes
in the AST.

## Compound Types

### `[]T` -- List

An ordered collection of values of type `T`:

```
// In a model definition
domains []string
listeners []listener
members []role
```

For collections whose elements must be unique, use a set type instead —
duplicates are rejected at validation (`NML2030`):

```
regions set<string>
```

### `T?` -- Optional

Marks a field as optional. Without `?`, fields are required by default.

```
// In a model definition
description string?       // may be omitted
siteName string            // must be present
faviconUrl path?           // may be omitted
```

## Reference Types

### `@roleRef`

References a role or identity. Used in access control modifiers and member lists.

Built-in references:
- `@public` -- unauthenticated access
- `@private` -- authenticated access
- `@anyone` -- all access (authenticated or not)
- `@loggedIn` -- authenticated users
- `@admin` -- administrative access

User-defined references follow the pattern `@namespace/path`:
- `@role/admin`
- `@user/gmatty@gmail.com`
- `@nudge:research/admin`

Role references compose with the conjunction operator `&`
(see [syntax.md](syntax.md) §Role Conjunctions): `@role/admin & @role/editor`
is one value carrying both atoms — consumers assign the AND semantics.

### References vs. Inline Definitions

Fields accept both inline definitions and references to declarations
defined elsewhere in the file. The parser distinguishes them syntactically:
- `field = SomeName` -- reference
- `field:` with indented content -- inline
