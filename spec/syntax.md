# NML Syntax Specification

## Lexical Structure

### Source text

NML files are UTF-8 encoded text files with the `.nml` extension. The
character-level contract below is checked by a dedicated policy pass on
every parse, before structure is considered; its governing principle is
**raw is transport, escaped is content**.

**Line endings** are LF or CRLF. The two transcriptions of a document are
the *same document*: line endings are transport, not content, so CRLF
normalizes to LF inside multiline string values, and no value can observe
which convention the file used (a fuzzed property test holds the parser to
this). A carriage return with no following line feed is an error
([NML0016](../crates/nml-core/assets/error-index.md#nml0016)) with a
machine-applicable deletion fix. This fence is re-transcribed to CRLF by
the docs harness before it runs, making the claim executable:

```nml check eol=crlf
service Api:
    port = 8080
    motd = """
        same value
        on every checkout
        """
```

**Raw source is printable.** C0 control characters (other than tab and
line endings) and DEL are rejected wherever they appear — values,
comments, or between tokens
([NML0017](../crates/nml-core/assets/error-index.md#nml0017)). Control
characters are content, and content belongs in `\u{…}` escapes, where
review can see it. Tab is legal raw in string content; indentation
restricts it separately (NML0005).

**Nothing invisible may steer the reader.** The bidirectional control
characters U+202A–U+202E and U+2066–U+2069 (the Trojan Source attack,
CVE-2021-42574) and interior U+FEFF are rejected in raw form
([NML0018](../crates/nml-core/assets/error-index.md#nml0018)); the banned
set matches rustc's. Right-to-left text itself is unaffected — Hebrew and
Arabic values need no bidi controls. A *leading* U+FEFF is accepted as a
byte-order mark for Windows-editor interoperability.

Every banned character has exactly one sanctioned spelling — its escape —
so no capability is lost, and intent becomes visible in review.

### Comments

Single-line comments begin with `//` and extend to the end of the line:

```
// this is a comment
service MyService:  // inline comment
    localMount = "/"
```

### Whitespace and Indentation

NML uses indentation to define structure. The canonical indentation unit is **4 spaces**.
Tabs are not permitted.

The lexer emits synthetic `INDENT` and `DEDENT` tokens based on indentation level changes,
similar to Python's tokenizer.

### Identifiers

Identifiers name declarations and fields. They must begin with a letter or underscore and
may contain letters, digits, underscores, and hyphens:

```
identifier = [a-zA-Z_][a-zA-Z0-9_-]*
```

### String Literals

Strings are enclosed in double quotes:

```
"hello world"
"/api/v1/{*}"
```

**Multiline strings** use triple quotes (`"""`) and support dedent (strip common leading whitespace from each line):

```
system = """
    You are an intent classifier.
    Analyze the user's message.
    """
```

The content is dedented: the minimum leading indent is stripped from each line. The newline immediately after the opening `"""` and before the closing `"""` is trimmed (TOML-style). Line endings in the body normalize to LF in the value — CRLF is transport, not content (see [Source text](#source-text)).

Escape sequences (same for single and multiline strings):
- `\"` -- literal double quote
- `\\` -- literal backslash
- `\n` -- newline
- `\t` -- tab
- `\r` -- carriage return (the only way to put a CR in a value)
- `\u{…}` -- 1–6 hex digits naming a Unicode scalar (as in Rust and
  Swift), e.g. `\u{E9}` é, `\u{1F389}` 🎉. Surrogates and code points
  above `10FFFF` are rejected. This is the sanctioned spelling for every
  character the [Source text](#source-text) policy bans raw.

### Template Expressions

Strings may contain **template expressions** delimited by double braces:

```
"Hello, {{args.name}}!"
"Welcome to {{config.appName}}"
```

A template expression has the form `{{namespace.key}}` where:
- `namespace` identifies the value source (e.g. `args`, `config`, `env`)
- `key` is the variable name within that namespace (may contain dots for nested access)

Template expressions are preserved as `TemplateString` nodes in the AST. The host
application is responsible for resolving them at runtime.

Double braces `{{...}}` are distinct from single-brace path variables `{name}` and
`{*}` which appear in path-typed values.

### Fallback Values

Any value can have a **fallback chain** using the pipe operator `|`:

```
apiKey = $ENV.API_KEY | $ENV.FALLBACK_KEY | "dev-default"
port = $ENV.PORT | 3000
```

The resolver evaluates fallbacks left-to-right: if the primary value cannot be
resolved (e.g. an unset environment variable), the next value is tried. The final
value in the chain is used if all preceding values fail.

Fallbacks produce a `Fallback(primary, fallback)` node in the AST and can be
chained to arbitrary depth.

### Number Literals

Numbers are unquoted decimal values:

```
8000        // integer
3.14        // decimal
0           // zero
-1          // negative
```

### Boolean Literals

```
true
false
```

### Money Literals

A decimal value followed by a space and an ISO 4217 currency code (3 uppercase letters):

```
19.99 USD
6.55 GBP
1299 JPY
```

### Duration Literals

Duration strings represent time spans. They are quoted strings with an
**unsigned integer** immediately followed by one unit suffix (no sign, no
decimals, no compound forms like `"1h30m"`); violations are `NML2029`:

```
"72h"       // 72 hours
"30m"       // 30 minutes
"5s"        // 5 seconds
"500ms"     // 500 milliseconds
```

### Path Literals

Path strings represent URL paths. They are quoted strings that may contain
variable placeholders `{name}` and wildcards `{*}`:

```
"/"
"/home"
"/user/{username}"
"/assets/{*}"
"/{org}/admin/{dept}/update"
```

### Secret References

Secret values are resolved from the environment or a vault at runtime.
They use the `$ENV.` prefix:

```
$ENV.MY_SECRET
$ENV.POSTMARK_SERVER_TOKEN
```

### Role References

Role and identity references use the `@` prefix:

```
@admin
@public
@private
@anyone
@loggedIn
@role/admin
@user/gmatty@gmail.com
@nudge:research/admin
@nudge/{org}/admin/{dept}/update
```

### Role Conjunctions

A single `&` between role references forms a **role-conjunction
expression** — one value carrying every atom (RFC 0014):

```
gate = @role/a & @role/b
|allow = [@role/admin & @role/editor, @plan/Pro]
|allow:
    - @member/acme & @role/billing
```

`&` is not a role-path character, so an unquoted `&` always terminates
the preceding role token: `@role/a&@role/b` lexes identically to
`@role/a & @role/b`. Conjunction binds tighter than a fallback chain and
is accepted wherever a role value appears — scalar values, inline array
elements, and block-list items. Arm selectors (`@selector -> target`)
name exactly one selector and do not accept conjunctions.

**Canonical form (normative):** a conjunction's value text is its atoms
joined with `" & "` — single spaces, regardless of source spacing. The
value layer lowers to this form and the formatter re-renders from it;
anything that prints selectors emits it, so a printed selector is always
valid source. The language carries the expression opaquely; consumers
assign the AND semantics.

Malformed conjunctions are parse errors with targeted guidance: a
dangling `&` ("expected a selector after '&'") and `&&` ("'&' is the
conjunction operator; '&&' is not needed").

## Structural Syntax

### Const Declarations

File-level constants use `const Name = value`. The value can be inline or on the next line (for long strings):

```
const Port = 8000
const ClassifierPrompt = "You are a classifier."

const LongPrompt =
    """
    You are an intent classifier for a recipe assistant.
    Analyze the user's message and determine their intent.
    """
```

References to consts use the bare identifier: `system = ClassifierPrompt`. The resolver substitutes the const's value.

### Template Declarations

Template declarations define named string values, typically for long text content:

```
template ClassifierPrompt:
    """
    You are an intent classifier for a recipe assistant.
    Analyze the user's message and determine their intent.
    """
```

The value must be a string (regular or multiline). Template declarations can
contain `{{...}}` expressions. They are accessed via `Document::template_value()`.

### Field Definitions (in Models and Traits)

Inside `model` and `trait` blocks, fields are defined using space-separated
`name type` syntax:

```
model service:
    host string
    port number
    debug bool?
    tags []string
    method httpMethod = "GET"
```

| Syntax | Meaning |
|--------|---------|
| `name type` | Required field |
| `name type?` | Optional field |
| `name type = value` | Field with default value |
| `name []type` | List-typed field |

Field definitions produce `FieldDefinition` nodes in the AST. Note that `:` after
a field name would start a nested block, not a type annotation.

### Top-Level Declarations

A declaration consists of a keyword, a name, a colon, and an indented body:

```
keyword Name:
    body...
```

Keywords are either built-in (`model`, `trait`, `enum`, `oneof`, `role`) or user-defined via models.

### Properties (Key-Value Pairs)

Properties assign a value to a field name using `=`:

```
localMount = "/"
address = ":8000"
debug = true
maxRetries = 3
price = 19.99 USD
```

### Nested Blocks

A field followed by `:` and an indented body defines a nested object:

```
rootProfile:
    domain = "dev.nudge.io:8000"

urlRoutes:
    homeRoute = "/"
    postLoginRoute = "/home"
```

### Lists

List items are prefixed with `- ` (dash-space):

```
domains:
    - "dev.nudge.io"
    - "example.com"
```

### Array Declarations

A `[]` prefix on a keyword declares an array of typed items:

```
[]resource registrationResources:
    - DefaultIndex:
        path = "/"
    - UserHome:
        path = "/home"
```

### Named List Items

List items can have names, creating addressable entries:

```
- SecureListener:
    address = ":8000"
    tls = "dev:auto"
```

### Bare Scalar List Items (Positional)

When a model marks a field as positional (`path path+`), a bare scalar list
item fills that field:

```
- "/test/matt"          // fills the positional field: path = "/test/matt"
```

### Reference List Items

A bare identifier in a list references an instance defined elsewhere:

```
- AuthDevByPass         // reference to a resource named AuthDevByPass
```

### Access Control Modifiers (`|` prefix)

The `|` prefix marks access control modifiers:

```
// Inline form
|allow = [@public, @role/admin]
|deny = []

// Block form
|allow:
    - @role/admin
    - @public
```

### Shared Properties (`.` prefix)

The `.` prefix on a property sets a default value inherited by the list
items of the body it appears in. **Every body is its own scope, at any
nesting depth**: an array declaration's shared properties apply to that
array's items; a nested list field's shared properties apply to that nested
list's items; an item's own body may declare shared properties for lists
inside it. Scopes never leak into siblings or parents.

Precedence, weakest to strongest: schema default → shared property → the
item's own entry. Bodyless scalar items are bare values with no fields, so
shared properties never apply to them (a scalar item *with* a body
participates like a named item).

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

### Reference Assignment

A field assigned to an unquoted identifier references an instance defined elsewhere:

```
resources = registrationResources
webServer = DefaultWebServer
```

The parser distinguishes references from string values by the absence of quotes.

## Grammar (PEG)

```peg
File            <- Declaration* EOF
Declaration     <- ConstDecl / TemplateDecl / ArrayDecl / BlockDecl
ConstDecl       <- "const" Identifier "=" ValueOrFallback NEWLINE
TemplateDecl    <- "template" Identifier ":" NEWLINE? StringLiteral NEWLINE
ArrayDecl       <- "[]" Keyword Identifier ":" NEWLINE INDENT ArrayBody DEDENT
BlockDecl       <- Keyword Identifier ":" NEWLINE INDENT Body DEDENT
Body            <- (FieldDef / Property / NestedBlock / Modifier / SharedProp
                   / ListItem / Comment / NEWLINE)*
ArrayBody       <- (ListItem / Modifier / SharedProp / Property / Comment / NEWLINE)*
FieldDef        <- Identifier FieldType "?"? ("=" ValueOrFallback)? NEWLINE
FieldType       <- "[]" Identifier / Identifier
ListItem        <- "-" (NamedItem / ShorthandItem / ReferenceItem / RoleExpr)
NamedItem       <- Identifier ":" NEWLINE INDENT Body DEDENT
ShorthandItem   <- StringLiteral NEWLINE
ReferenceItem   <- Identifier NEWLINE
Property        <- Identifier "=" ValueOrFallback NEWLINE
NestedBlock     <- Identifier ":" NEWLINE INDENT Body DEDENT
Modifier        <- "|" Identifier "=" ValueOrFallback NEWLINE
                 / "|" Identifier ":" NEWLINE INDENT ListBody DEDENT
                 / "|" Identifier FieldType "?"? NEWLINE
SharedProp      <- "." Identifier ":" NEWLINE INDENT Body DEDENT
ListBody        <- (ListItem / Comment / NEWLINE)*
ValueOrFallback <- Value ("|" Value)*
Value           <- MoneyLiteral / NumberLiteral / BoolLiteral / StringLiteral
                 / SecretRef / ArrayLiteral / RoleExpr / Identifier
MoneyLiteral    <- Decimal CurrencyCode
NumberLiteral   <- "-"? [0-9]+ ("." [0-9]+)?
BoolLiteral     <- "true" / "false"
StringLiteral   <- '"""' MultilineContent '"""'
                 / '"' StringContent '"'
StringContent   <- (StringChar / TemplateExpr)*
TemplateExpr    <- "{{" [^}]+ "}}"
SecretRef       <- "$ENV." Identifier ("." Identifier)*
ArrayLiteral    <- "[" (Value ("," Value)*)? "]"
CurrencyCode    <- [A-Z]{3}
Decimal         <- "-"? [0-9]+ ("." [0-9]+)?
Identifier      <- [a-zA-Z_][a-zA-Z0-9_-]*
RoleExpr        <- RoleRef ("&" RoleRef)*
RoleRef         <- "@" RolePath
RolePath        <- [a-zA-Z0-9_/:@{}.+-]+
Keyword         <- Identifier
Comment         <- "//" [^\n]*
NEWLINE         <- "\n"
INDENT          <- <increase in indentation level>
DEDENT          <- <decrease in indentation level>
EOF             <- <end of input>
```
