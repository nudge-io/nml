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

General-purpose numeric type covering both whole numbers and decimals. Use the
`<integer>` constraint when a field must be a whole number.

```
port = 8000
weight = 0.75
maxRetries = 3
```

There is no separate `int` or `float` type. The int/float distinction is an
implementation detail that does not belong in a configuration language. Constraints
handle validation:

```
// In a model definition
port number <integer, min = 1, max = 65535>
weight number <min = 0.0, max = 1.0>
```

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

Quoted time duration strings with a numeric value and unit suffix:

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

## Compound Types

### `[]T` -- List

An ordered collection of values of type `T`:

```
// In a model definition
domains []string
listeners []listener
members []@roleRef
```

Use the `<distinct>` constraint to require all items be unique:

```
domains []string <distinct>
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

### `&T` -- Reference Only

Restricts a field to accept only a reference to an instance defined elsewhere.
The field cannot be defined inline.

```
// In a model definition
redirectTo &listener?      // must point to an existing listener
```

By default (without `&`), all fields accept both inline definitions and references.
The parser distinguishes them syntactically:
- `field = SomeName` -- reference
- `field:` with indented content -- inline

Use `&` only when inline definition does not make semantic sense.
