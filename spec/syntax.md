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
([NML0016](../crates/nml-core/assets/error-index.md#nml0016)) — reported
without a machine fix in token position (on a CR-terminated old-Mac file
every CR is a line ending, and deleting it would glue lines together);
inside a string the machine fix is the `\r` escape. This fence is
re-transcribed to CRLF by the docs harness before it runs, making the
claim executable:

```nml check eol=crlf
service Api:
    port = 8080
    motd = """
        same value
        on every checkout
        """
```

**Raw source is printable.** Every Unicode control character (general
category Cc — C0, DEL, and the C1 range U+0080–U+009F), other than tab
and line endings, is rejected wherever it appears — values, comments,
or between tokens
([NML0017](../crates/nml-core/assets/error-index.md#nml0017)). Control
characters are content, and content belongs in `\u{…}` escapes, where
review can see it; C1's one-byte CSI (U+009B) is the same
terminal-injection primitive as ESC. Tab is legal raw in string
content; indentation restricts it separately (NML0005).

**Nothing invisible may steer the reader.** The explicit bidirectional
controls U+202A–U+202E and U+2066–U+2069 (the Trojan Source attack,
CVE-2021-42574), interior U+FEFF, the U+2028/U+2029 line and
paragraph separators, and the Unicode tag block U+E0000–U+E007F (an
invisible ASCII mirror — raw, a hidden-text channel; emoji tag
sequences are written with escapes) are rejected in raw form
([NML0018](../crates/nml-core/assets/error-index.md#nml0018)). The bidi
set matches rustc's Trojan-Source lints; the whole set is a strict
superset of rustc's, per the display-vs-parse guidance of UTS #55 —
with the separators banned, every Unicode line-boundary character
outside LF/CRLF is diagnosed (NEL, VT and FF as controls; a bare CR by
its own rule, NML0016). Right-to-left text itself is unaffected — Hebrew and
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

Three rules make the value depend on transport shape alone (the Java
text-block order, JEP 378):

- **Dedent is computed on source lines, before escapes are interpreted.**
  An escaped `\n` produces a newline in the *value* without creating a
  line for indentation purposes, and escaped whitespace survives
  stripping — `\s` (or `\u{20}`) at the start of a line is protected content, not
  indentation (the capability Java added `\s` for).
- **Content must begin on the line after the opening `"""`**
  ([NML0019](../crates/nml-core/assets/error-index.md#nml0019), the
  Swift/Java rule) — text on the opening line would participate in the
  indent computation. Whitespace alone there is harmless and legal, as is
  the empty `""""""`; short values use ordinary `"…"` strings.
- **The formatter protects edge spaces**: when it renders a multiline
  value, a line's first and last space are emitted as `\s`, so neither
  reparse dedent nor an editor's trim-on-save can change the value.
- **An own-line closing `"""` aligns with the content**
  ([NML0020](../crates/nml-core/assets/error-index.md#nml0020),
  machine-fixable): with alignment enforced, the delimiter-anchored
  reading and the min-indent reading provably agree on every accepted
  document.
- **Tabs may not appear in a body line's indentation** (NML0005's rule,
  extended): column arithmetic must not depend on editor settings,
  inside strings as outside. Tabs as *content* (mid-line, or via `\t`)
  are unaffected.

Escape sequences (same for single and multiline strings):
- `\"` -- literal double quote
- `\\` -- literal backslash
- `\n` -- newline
- `\t` -- tab
- `\r` -- carriage return (the only way to put a CR in a value)
- `\s` -- space (as in Java text blocks): a *protected* space that
  survives multiline dedent and editor whitespace-trimming. The
  formatter emits it for a line's leading/trailing spaces.
- `\u{…}` -- 1–6 hex digits naming a Unicode scalar (as in Rust and
  Swift), e.g. `\u{E9}` é, `\u{1F389}` 🎉. Surrogates and code points
  above `10FFFF` are rejected. This is the sanctioned spelling for every
  character the [Source text](#source-text) policy bans raw.
- `\` before a line break (multiline strings only) -- line continuation,
  as in Java and Swift: the two source lines join without a newline in
  the value. Dedent applies first (each line's indent is stripped), then
  the join. A `\\` is a literal backslash, never a continuation, and a
  continuation on the last content line is an error (the string ends
  mid-escape).

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

Numbers are unquoted decimal values. `_` digit separators are permitted
between two digits — never leading, trailing, doubled, or dot-adjacent
(stricter than Rust: one spelling per grouping; violations are `NML0013`
with a machine-applicable strip fix). Separators are spelling, never
value: `nml fmt` canonicalizes them away, exactly as `007` → `7`.

```
8000        // integer
3.14        // decimal
0           // zero
-1          // negative
10_000      // separated (canonical form: 10000)
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

A duration is a **literal** (RFC 0017): one or more **unsigned integer**
components, each immediately followed by a unit suffix — `h`, `m`, `s`,
`ms`, `us`, or `ns`. Components may be attached (`1h30m`) or separated by
inline whitespace (`1h 30m`). The unit set is **closed at both ends by
construction**: `ns` is the resolution of the value domain itself,
and `h` is the largest exact unit (calendar units are permanently
excluded). The canonical form is attached with components ordered
coarse→fine (`1h30m`; `nml fmt` normalizes spacing and order).
No sign (`NML3006`), no decimals in source (`NML3005` — write the finer
unit or a compound: `1h30m`, not `1.5h`), no duplicate units in source
(`NML3007` — `1h2h` merges to `3h`), no dangling magnitudes (`NML3008`),
and no calendar units (`d`, `w`):

```
72h         // 72 hours
1h30m       // compound: 1 hour 30 minutes
5m2s        // compound: 5 minutes 2 seconds
30m         // 30 minutes
5s          // 5 seconds
500ms       // 500 milliseconds
250us       // 250 microseconds (ASCII `us`; `µ` is not source-legal)
1_000ns     // 1000 nanoseconds (digit separators, as in numbers)
0s          // zero is valid
```

Units are lowercase; a currency code is exactly 3 uppercase letters, so
the two suffix families are disjoint by construction (`30S` and `30x`
are `NML3004`, with a nearest-unit fix). Comparison is **semantic**:
`30s` equals `30000ms` equals `30_000_000us` — a reload diff between
spellings is no change. The total must fit the runtime duration domain
(up to `u64::MAX` seconds); beyond it is `NML3006`.

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
assign the AND semantics. (Consumers may additionally define a
quoted-value form for selector values that literally contain ` & ` —
nudge does, RFC 0055 D11 — but that form is only expressible as an NML
*string* value: the role-token charset has no space or quote, so bare
tokens and this canonical form are unaffected.)

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
| `name type+` | Positional field — a bare scalar list item fills it |
| `name type = value` | Field with default value |
| `name []type` | List-typed field |
| `name set<type>` | Set-typed field — unordered, unique elements |
| `name (a \| b)` | Union-typed field — one of several types |
| `name (k -> v)` | Arm-set field — ordered, first-match `selector -> target` |
| `name number(...)` | Numeric facets — see below |

Field definitions produce `FieldDefinition` nodes in the AST. Note that `:` after
a field name would start a nested block, not a type annotation.

The union pipe and the arm arrow are only ever consumed **inside** parentheses.
A bare `k -> v` at type position is a parse error, which is what keeps the
field suffixes (`?`, `+`) unambiguous — they always bind to the field, never to
the last type inside the parens.

### Numeric Facets (RFC 0018)

A facet list may follow a type **name**, constraining the values that type
admits. It is part of the type, so it travels wherever the name appears —
inside `[]`, inside `set<>`, and in union variants:

```nml check
model server:
    port number(min = 1, max = 65535)
    weight number(min = 0, exclusiveMax = 1)?
    priceStep number(multipleOf = 0.01)?
    ports set<number(min = 1)>?
```

The keys are `min` and `max` (inclusive), `exclusiveMin` and `exclusiveMax`,
and `multipleOf`. Values are number literals only — no references, no
expressions. A facet list never spans lines: a swallowed newline would absorb
the following field into the type, so the parser stops at the line break and
says so.

Parse-level, a facet list is accepted after *any* type name. "Facets attach
only to `number`" is a schema-load rule with its own diagnostic (`NML2058`),
not a parse error — recovery keeps the tree structured and the finding
singular. Enforcement of a declared range against a value is `NML2057`, exact
through the decimal core.

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

List items are prefixed with `- ` (dash-space). Each element is a
**single value**: fallback chains (`a | b`) are property-position only
([NML0021](../crates/nml-core/assets/error-index.md#nml0021) — name a
chain with a `const` and reference it to use one as an element).


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
FieldDef        <- Identifier FieldType ("?" / "+")? ("=" ValueOrFallback)? NEWLINE
FieldType       <- "[]" FieldType
                 / "(" FieldType ("->" FieldType / ("|" FieldType)*) ")"
                 / "set" "<" FieldType ("|" FieldType)* ">"
                 / Identifier FacetList?
FacetList       <- "(" Facet ("," Facet)* ")"      # no trailing comma
Facet           <- Identifier "=" NumberLiteral    # key set checked at schema load
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
Value           <- MoneyLiteral / DurationLiteral / NumberLiteral / BoolLiteral
                 / StringLiteral / SecretRef / ArrayLiteral / RoleExpr / Identifier
MoneyLiteral    <- Decimal CurrencyCode
DurationLiteral <- Digits DurationUnit
DurationUnit    <- "ms" / "us" / "ns" / "h" / "m" / "s"
NumberLiteral   <- "-"? Digits ("." Digits)?
Digits          <- [0-9] ([0-9_]* [0-9])?          # `_` only between two digits
BoolLiteral     <- "true" / "false"
StringLiteral   <- '"""' MultilineContent '"""'
                 / '"' StringContent '"'
StringContent   <- (Escape / TemplateExpr / StringChar)*
MultilineContent<- (Escape / LineContinuation / TemplateExpr / StringChar / NEWLINE)*
Escape          <- "\" (["\ntrs] / "u{" [0-9A-Fa-f]{1,6} "}")
LineContinuation<- "\" NEWLINE
TemplateExpr    <- "{{" [^}]+ "}}"
SecretRef       <- "$ENV." Identifier ("." Identifier)*
ArrayLiteral    <- "[" (Value ("," Value)*)? "]"
CurrencyCode    <- [A-Z]{3}
Decimal         <- "-"? Digits ("." Digits)?
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
