# NML Language Guide

This guide covers everything you can express in a `.nml` file.

## File Basics

NML files are UTF-8 text with the `.nml` extension. Structure is defined by
**indentation** (4 spaces per level). Tabs are not allowed.

Comments use `//`:

```
// full-line comment
service MyService:  // inline comment
```

## Declarations

Every `.nml` file contains one or more **declarations**. A declaration has a
keyword, a name, a colon, and an indented body:

```
keyword Name:
    body...
```

Keywords are either built-in (`model`, `trait`, `enum`, `roleTemplate`) or
user-defined via models (e.g. `service`, `resource`).

### Array Declarations

Prefix a keyword with `[]` to declare a named list of typed items:

```
[]resource registrationResources:
    - HomePage:
        path = "/"
    - UserProfile:
        path = "/user/{*}"
```

## Properties

Properties assign values using `=`:

```
localMount = "/"
address = ":8000"
debug = true
maxRetries = 3
price = 19.99 USD
```

## Nested Blocks

A field followed by `:` and an indented body defines a nested object:

```
rootProfile:
    domain = "dev.nudge.io:8000"
    protocol = "https"
```

## Lists

List items are prefixed with `- `:

```
domains:
    - "dev.nudge.io"
    - "example.com"
```

### Named List Items

List items can have names, making them addressable:

```
- SecureListener:
    address = ":8000"
    tls = "dev:auto"
```

### Reference List Items

A bare identifier in a list references an instance defined elsewhere:

```
- AuthDevByPass
```

### Shorthand List Items

When a model defines a `<shorthand>` field, bare strings expand to that field:

```
- "/test/matt"    // expands to: path = "/test/matt"
```

## Types

### Primitive Types

| Type | Syntax | Example |
|------|--------|---------|
| `string` | Quoted text | `"hello world"` |
| `number` | Unquoted decimal | `8000`, `3.14`, `-1` |
| `money` | Amount + currency code | `19.99 USD`, `1299 JPY` |
| `bool` | Unquoted | `true`, `false` |
| `duration` | Quoted with unit | `"72h"`, `"30s"`, `"500ms"` |
| `path` | Quoted URL path | `"/"`, `"/user/{username}"`, `"/assets/{*}"` |
| `secret` | Environment reference | `$ENV.API_KEY` |

#### Money

Money literals pair an exact decimal amount with an ISO 4217 currency code.
They are stored as integer minor units internally, avoiding floating-point
precision issues:

```
monthlyPrice = 19.99 USD   // stored as 1999 minor units
japanPrice = 1299 JPY       // stored as 1299 (JPY has 0 decimal places)
```

Invalid precision is a parse error -- `19.999 USD` is rejected because USD
allows at most 2 decimal places.

#### Duration Units

| Unit | Meaning |
|------|---------|
| `h` | hours |
| `m` | minutes |
| `s` | seconds |
| `ms` | milliseconds |

#### Path Variables

Paths support named placeholders `{name}` and wildcards `{*}`:

```
"/user/{username}"
"/assets/{*}"
"/{org}/admin/{dept}/update"
```

#### Secrets

Secret values are resolved at runtime and masked in logs:

```
serverToken = $ENV.POSTMARK_SERVER_TOKEN
```

### Compound Types

| Syntax | Meaning |
|--------|---------|
| `[]T` | List of type `T` |
| `T?` | Optional field (may be omitted) |

### Reference Types

| Syntax | Meaning |
|--------|---------|
| `@roleRef` | Role or identity reference |
| `&T` | Reference-only (must point to an existing instance) |

By default, fields accept both inline definitions and references. Use `&T` to
restrict a field to reference-only when inline definition doesn't make sense.

## Modeling

### Defining Models

Models define the shape of configuration objects. Once defined, the model name
becomes a keyword:

```
model service:
    localMount path
    resources []resource
    endpoints []endpoint
```

```
// Now "service" is a keyword
service NudgeService:
    localMount = "/"
    resources = registrationResources
    endpoints = registrationEndpoints
```

### Field Presence

| Syntax | Meaning |
|--------|---------|
| `fieldName type` | Required -- must be provided |
| `fieldName type?` | Optional -- may be omitted |
| `fieldName type = value` | Default -- used when omitted |

```
model webProfile:
    siteName string               // required
    description string?           // optional
    sessionDuration duration = "24h"  // has default
```

### Constraints

Constraints are specified in angle brackets after the type:

```
port number <integer, min = 1, max = 65535>
email string <pattern = "^[^@]+@[^@]+$">
domains []string <distinct>
price money <currency = "USD">
```

| Constraint | Applies to | Purpose |
|------------|-----------|---------|
| `<unique>` | any | Value must be unique across all instances |
| `<secret>` | string | Masked in logs, supports `$ENV.X` resolution |
| `<token>` | string | Used as a lookup identifier |
| `<distinct>` | lists | All items must be unique |
| `<shorthand>` | any | Bare string list items expand to this field |
| `<integer>` | number | Must be a whole number |
| `<min = N>` | number | Minimum value (inclusive) |
| `<max = N>` | number | Maximum value (inclusive) |
| `<minLength = N>` | string | Minimum length |
| `<maxLength = N>` | string | Maximum length |
| `<pattern = "re">` | string | Must match regex |
| `<currency = "X">` | money | Restrict accepted currency codes |

### Traits

Traits are reusable groups of fields that can be mixed into models:

```
trait accessControlled:
    |allow []@roleRef
    |deny []@roleRef

model resource (accessControlled):
    path path <shorthand>
    method httpMethod = "GET"
```

Multiple traits are comma-separated:

```
model service (accessControlled, auditable):
    localMount path
```

### Enums

Enums restrict a field to a fixed set of string values:

```
enum httpMethod:
    - "GET"
    - "POST"
    - "PUT"
    - "DELETE"
    - "PATCH"

model resource:
    method httpMethod = "GET"
```

### Inline Nested Objects

For one-off structures that don't need their own model:

```
model accessControl:
    sessionDuration duration = "24h"

    urlRoutes:
        homeRoute path = "/"
        postLoginRoute path
        postLogoutRoute path
```

### Shared Properties

The `.` prefix defines a property inherited by all children of a list:

```
[]endpoint registrationEndpoints:
    .healthCheck:
        path = "/health"

    - Reg1:
        address = "http://localhost:8004"
    - Reg2:
        address = "localhost:8001"
```

All endpoints inherit `.healthCheck` unless they override it.

## Access Control

The `|` prefix marks access control modifiers.

### `|allow`

Defines which roles are permitted. Unlisted roles are implicitly denied.

### `|deny`

Explicitly denies access, even if `|allow` would permit it. `|deny` takes
precedence.

### Inline Form

```
|allow = [@public, @role/admin]
|deny = []
```

### Block Form

```
|allow:
    - @role/admin
    - @public
```

### Built-in Roles

| Role | Meaning |
|------|---------|
| `@public` | Unauthenticated access |
| `@private` | Authenticated access |
| `@anyone` | All access |
| `@loggedIn` | Authenticated users (alias for `@private`) |
| `@admin` | Administrative access |

### User-Defined Roles

```
@role/admin
@role/pro-user
@user/gmatty@gmail.com
@nudge:research/admin         // org:path format
@nudge/{org}/admin/{dept}/update  // parameterized
```

### Inheritance

Access control flows from parent to child. A `service` sets top-level rules;
individual resources or endpoints can override them:

```
service NudgeService:
    |allow:
        - @role/admin

    resources:
        - PublicPage:
            |allow = [@public]    // overrides service-level
            path = "/"

        - AdminPage:
            // inherits @role/admin from service
            path = "/admin"
```

## Reference Assignment

Assigning an unquoted identifier references an instance defined elsewhere:

```
resources = registrationResources
webServer = DefaultWebServer
```

Quoted values are strings; unquoted identifiers are references.

## File Conventions

| Pattern | Purpose |
|---------|---------|
| `*.model.nml` | Model, trait, and enum definitions |
| `*.service.nml` | Service instance declarations |
| `*.nml` | General configuration |

Models are loaded first, then instance files are validated against them.
