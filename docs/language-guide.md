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

Keywords are either built-in (`model`, `trait`, `enum`, `oneof`, `role`) or
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

## Constants

Constants define reusable values at the file level:

```
const MaxRetries = 3
const DefaultGreeting = "Welcome!"
```

For long strings, the value can go on the next line:

```
const SystemPrompt =
    """
    You are an intent classifier for a recipe assistant.
    Analyze the user's message and determine their intent.
    """
```

Reference a constant by using its bare name as a value:

```
service MyApp:
    greeting = DefaultGreeting
    retries = MaxRetries
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

### Positional List Items

When a model marks one field as positional (`path path+`), a bare scalar
list item fills that field:

```
- "/test/matt"    // fills the positional field: path = "/test/matt"
```

## Types

### Primitive Types

| Type | Syntax | Example |
|------|--------|---------|
| `string` | Quoted text | `"hello world"` |
| `number` | Unquoted decimal | `8000`, `3.14`, `-1`, `10_000` |
| `money` | Amount + currency code | `19.99 USD`, `1299 JPY` |
| `bool` | Unquoted | `true`, `false` |
| `duration` | Unquoted with unit(s) | `72h`, `30s`, `500ms`, `1h30m` |
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
| `us` | microseconds |
| `ns` | nanoseconds |

Durations are typed literals: one or more unsigned integers, each
attached to its unit, coarse to fine (`30s`, `1h30m` — never `"30s"` or
`1.5h`; a fractional magnitude gets a fix suggesting the exact
respelling at the same granularity, like `1h30m` for `1.5h`). The
authored units are kept — `72h` formats as `72h`, and `90m` never
becomes `1h30m` — while comparison is by value, so `30s` and `30000ms`
(or `90m` and `1h30m`) are the same duration to sets and reload diffs.

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

### Multiline Strings

Multiline strings use triple double-quotes (`"""`). The content is dedented
(minimum leading indent is stripped) and surrounding newlines are trimmed:

```
const SystemPrompt =
    """
    You are a helpful assistant.
    Be concise and accurate.
    """
```

Single-line escape sequences work in both regular and multiline strings:
`\"` (literal quote), `\\` (literal backslash), `\t` (tab), `\n` (newline).

### Template Expressions

Strings can contain `{{namespace.key}}` expressions for dynamic interpolation.
Template expressions are preserved in the AST and resolved at runtime by the
host application:

```
instructions = "You are {{args.persona}}. Help the user with {{args.topic}}."
greeting = "Welcome to {{config.appName}}!"
```

The namespace prefix (e.g. `args`, `config`, `env`) identifies the source of
the value. Valid namespaces are configured per-project via `nml-project.nml`
or through the host application.

Template expressions use double braces `{{...}}` to distinguish them from
single-brace path variables like `/user/{id}`.

### Fallback Values

Any value can have a fallback chain using `|`. If the primary value cannot be
resolved (e.g. an environment variable is not set), the next value in the
chain is used:

```
apiKey = $ENV.API_KEY | $ENV.FALLBACK_KEY | "dev-key-default"
port = $ENV.PORT | 3000
```

Fallbacks work with any value type -- secrets, strings, numbers, and
references.

### Compound Types

| Syntax | Meaning |
|--------|---------|
| `[]T` | List of type `T` — ordered, duplicates allowed |
| `set<T>` | Set of type `T` — unordered, duplicates rejected at load |
| `T?` | Optional field (may be omitted) |
| `T+` | Positional field — a bare scalar list item fills it |
| `(A \| B)` | Union — the value may be either type |
| `(K -> V)` | Arm set — ordered, first-match `selector -> target` |
| `number(...)` | Numeric facets — see [Constraining Values](#constraining-values-numeric-facets) |

`[]T` and `set<T>` differ in meaning, not just in checking: list order is
content (it survives diffs), set order is incidental (diffs are
order-insensitive, and the authored order is preserved in source but carries
no meaning).

The union pipe and the arm arrow live **inside** their parentheses. That is
what keeps the field suffixes unambiguous — in `landing (string | (role ->
string))?`, the `?` can only be describing the field.

Arm targets take three forms: a reference (`-> StatusPage`), a string
literal for scalar targets (`-> "fallback"`), or an inline model instance
(`-> adminLanding:` followed by an indented body). Selectors may be roles
(`@role/admin`), the `else` catch-all, or a string key (`"plan"`).

```
model landingPage:
    label number

model service:
    routing (role -> landingPage)?

service Api:
    routing:
        @role/admin -> adminLanding:
            label = 4
        "plan" -> "upsell"
```

Enum-typed selectors use the same string-key form — `"pro" -> Target` when
`K` is an enum:

```
enum planKind:
    - "free"
    - "pro"

model service:
    routing (planKind -> string)?

service Api:
    routing:
        "pro" -> "upsell"
```

The validator resolves `V` against whichever form you choose; a quoted name
after `->` is always a literal, never an inline block (`-> "name":` is a
parse error).

### Reference Types

| Syntax | Meaning |
|--------|---------|
| `@roleRef` | Role or identity reference |
| `@a & @b` | Role-conjunction expression (RFC 0014) |

Fields accept both inline definitions and references to declarations
defined elsewhere in the file.

A single `&` between role references forms one conjunction expression
(valid in scalar values, inline array elements, and block-list items); see
[Requiring Several Roles at Once](#requiring-several-roles-at-once-) for
usage and semantics. RFC 0011 specifies the grammar.

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
    sessionDuration duration = 24h  // has default
```

### Constraining Values (Numeric Facets)

A `number` field can carry its own valid range, written into the type rather
than checked later in your code:

```nml check
model server:
    port number(min = 1, max = 65535)
    weight number(min = 0, exclusiveMax = 1)?
    priceStep number(multipleOf = 0.01)?
    ports set<number(min = 1)>?
```

| Facet | Meaning |
|-------|---------|
| `min` | Value must be greater than or equal |
| `max` | Value must be less than or equal |
| `exclusiveMin` | Value must be strictly greater |
| `exclusiveMax` | Value must be strictly less |
| `multipleOf` | Value must be an exact multiple |

Facets belong to the type, so they follow the type name everywhere it appears
— `[]number(min = 0)`, `set<number(min = 1)>`, and union variants all
constrain each element. Field defaults are held to the same rule: a default
that violates its own facets is an error at schema load, not a surprise at
runtime.

Enforcement is **exact**, through the same decimal core that stores your
numbers. `0.3` is a multiple of `0.1` here (float-based validators famously
disagree), boundary values behave the way the schema reads, and `80.0`
satisfies an integer bound because it *is* 80.

Values must be number literals — no references, no expressions. A schema is a
contract, not a program. A facet list also never spans lines.

Two diagnostics come with them: `NML2057` when a value violates a declared
facet, and `NML2058` when the declaration itself is invalid — facets on a
non-`number` type, an unknown or duplicate key, `min` and `exclusiveMin` (or
the `max` pair) together, an unsatisfiable range, or a `multipleOf` that is
not positive.

One thing facets deliberately do not cover: `$ENV`-backed values bypass them,
exactly as they bypass every other static schema check, because resolution
happens after validation.

### Traits

Traits are reusable groups of fields that models mix in with `is`. Unlike
a model, a trait can never be instantiated as a block, used as a field
type, or targeted by a `oneof` arm — it is composition-only (RFC 0011,
`NML2020`–`NML2024`):

```nml check
trait accessControlled:
    |allow []role
    |deny []role

model resource is accessControlled:
    path path+
    method httpMethod = "GET"

enum httpMethod:
    - "GET"
    - "POST"
```

Multiple traits are comma-separated after `is`, and traits may compose
other traits. Inherited fields keep their defaults; a model's own field
overrides a same-named inherited one:

```nml check
trait accessControlled:
    |allow []role?
    |deny []role?
trait auditable:
    auditLog string?

model service is accessControlled, auditable:
    localMount path
```

Instantiating a trait (`accessControlled X:`) is an error even in lenient
validation — the schema knows the name is composition-only.

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

### Discriminated Unions (`oneof`)

A `oneof` selects one of several variant models by the value of a
**discriminator** field. It is the schema-level expression of a tagged union:
each arm binds a discriminator value to a variant model, and an instance block
carries the discriminator flat alongside the selected variant's fields.

```
model emailLog:

model emailPostmark:
    fromAddress string
    serverToken secret
    messageStream string = "outbound"

oneof email by provider:
    "log"      -> emailLog
    "postmark" -> emailPostmark
```

A matching instance sets the discriminator and the chosen variant's fields:

```
email Outbound:
    provider = "postmark"
    fromAddress = "no-reply@example.com"
    serverToken = $ENV.POSTMARK_TOKEN
```

The validator resolves `provider` to a variant and enforces **that variant's**
required and unknown fields: a missing `serverToken`, or a `serverToken` placed
on a `"log"` block, is rejected at validation time. The discriminator itself is
owned by the union, so variant models do not redeclare it.

#### Default discriminator

A `oneof` may declare a **default arm** with `= "value"` after the `by` clause,
mirroring field-default syntax. When an instance omits the discriminator, the
default is injected and that variant's defaults are applied — so a fully-defaulted
block needs no discriminator at all:

```
oneof email by provider = "log":
    "log"      -> emailLog
    "postmark" -> emailPostmark
```

The default value must name one of the arms (checked at schema load), and must be
a quoted string. An explicitly authored discriminator always wins over the default.

#### Enum-typed discriminator

The discriminator may be typed by a declared `enum` with `as`, in which case the
arms must **exactly** cover the enum's variants — a missing variant or an arm outside
the enum is rejected at schema load (exhaustiveness):

```
enum providerKind:
    - "log"
    - "postmark"

oneof email by provider as providerKind = "log":
    "log"      -> emailLog
    "postmark" -> emailPostmark
```

The clauses compose left to right: `by <field> [as <enum>] [= <default>]`.

A `oneof` can be referenced anywhere a model can — as a block keyword, a nested
field (`email email?`), or a list element (`[]email`). Variant model names must
be unique, discriminator values must be distinct, and a name cannot be declared
as more than one of `model` / `enum` / `oneof`.

### Inline Nested Objects

For one-off structures that don't need their own model:

```
model accessControl:
    sessionDuration duration = 24h

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

### Requiring Several Roles at Once (`&`)

Join role references with `&` to require **all** of them in one selector:

```
service AdminReports:
    |allow:
        - @role/admin & @role/billing    // must hold BOTH
        - @role/owner                    // or this one alone
```

Each list entry is an independent grant (any entry may match); `&`
composes conditions *within* an entry — the list is the OR, `&` is the
AND. Consumers assign the set-intersection semantics. The syntax details
— canonical `" & "` form, spacing, and the `&&`/dangling-`&` errors —
live in "Role conjunctions" under Reference Types above.

### Inheritance

Access control flows from parent to child, and **children narrow — they
never widen**: nested gates mean the visitor passes *every* level, so a
child's `|allow` can tighten the audience but cannot punch a hole in the
parent's. (A `@public` child under an admins-only parent composes to
admins-only — the parent's gate always applies.)

```
service NudgeService:
    |allow:
        - @public             // the service is broadly reachable

    resources:
        - AdminPanel:
            |allow = [@role/admin]    // narrows: this resource is admins-only```

## Reference Assignment

Assigning an unquoted identifier references an instance defined elsewhere:

```
resources = registrationResources
webServer = DefaultWebServer
```

Quoted values are strings; unquoted identifiers are references.

## Template Declarations

Template declarations provide a named string value, typically used for long
text content like prompts:

```
template ClassifierPrompt:
    """
    You are an intent classifier.
    Analyze the user's message and determine their intent.
    Return one of: greeting, question, farewell, other.
    """
```

The value must be a string (regular or multiline). Template declarations can
contain `{{...}}` expressions just like any other string.

## Project Configuration

Create an `nml-project.nml` at your workspace root to configure NML tooling:

```
project MyProject:
    schema:
        - "schemas/service.model.nml"
        - "schemas/database.model.nml"
    templateNamespaces = ["env", "config", "args"]
    modifiers = ["allow", "deny", "readonly"]
    keywords = ["service", "database", "cache"]
```

This file is automatically detected by the NML language server and affects
schema validation, template namespace checking, modifier validation, and
keyword completions.

## File Conventions

| Pattern | Purpose |
|---------|---------|
| `*.model.nml` | Model, trait, and enum definitions |
| `*.workflow.nml` | Workflow definitions |
| `*.service.nml` | Service instance declarations |
| `*.nml` | General configuration |
| `nml-project.nml` | Project-level tooling configuration |

Models are loaded first, then instance files are validated against them.
