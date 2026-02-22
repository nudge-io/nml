# NML Syntax Specification

## Lexical Structure

### Encoding

NML files are UTF-8 encoded text files with the `.nml` extension.

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

Escape sequences:
- `\"` -- literal double quote
- `\\` -- literal backslash
- `\n` -- newline
- `\t` -- tab

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

Duration strings represent time spans. They are quoted strings with a numeric value
followed by a unit suffix:

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

## Structural Syntax

### Top-Level Declarations

A declaration consists of a keyword, a name, a colon, and an indented body:

```
keyword Name:
    body...
```

Keywords are either built-in (`model`, `trait`, `enum`) or user-defined via models.

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

### Bare String List Items (Shorthand)

When a model defines a `<shorthand>` field, bare string list items expand to that field:

```
- "/test/matt"          // equivalent to: - _: path="/test/matt"
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

The `.` prefix on a property within an array declaration sets a default value
inherited by all list children:

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
Declaration     <- ArrayDecl / BlockDecl
ArrayDecl       <- "[]" Keyword Identifier ":" NEWLINE INDENT ArrayBody DEDENT
BlockDecl       <- Keyword Identifier ":" NEWLINE INDENT Body DEDENT
Body            <- (Property / NestedBlock / Modifier / SharedProp / Comment / NEWLINE)*
ArrayBody       <- (ListItem / Modifier / SharedProp / Property / Comment / NEWLINE)*
ListItem        <- "-" (NamedItem / ShorthandItem / ReferenceItem)
NamedItem       <- Identifier ":" NEWLINE INDENT Body DEDENT
ShorthandItem   <- StringLiteral NEWLINE
ReferenceItem   <- Identifier NEWLINE
Property        <- Identifier "=" Value NEWLINE
NestedBlock     <- Identifier ":" NEWLINE INDENT Body DEDENT
Modifier        <- "|" Identifier "=" Value NEWLINE
                 / "|" Identifier ":" NEWLINE INDENT ListBody DEDENT
SharedProp      <- "." Identifier ":" NEWLINE INDENT Body DEDENT
ListBody        <- (ListItem / Comment / NEWLINE)*
Value           <- MoneyLiteral / NumberLiteral / BoolLiteral / StringLiteral
                 / SecretRef / ArrayLiteral / Identifier
MoneyLiteral    <- Decimal CurrencyCode
NumberLiteral   <- "-"? [0-9]+ ("." [0-9]+)?
BoolLiteral     <- "true" / "false"
StringLiteral   <- '"' StringChar* '"'
SecretRef       <- "$ENV." Identifier
ArrayLiteral    <- "[" (Value ("," Value)*)? "]"
CurrencyCode    <- [A-Z]{3}
Decimal         <- "-"? [0-9]+ ("." [0-9]+)?
Identifier      <- [a-zA-Z_][a-zA-Z0-9_-]*
RoleRef         <- "@" RolePath
RolePath        <- [a-zA-Z0-9_/:@{}.+-]+
Keyword         <- Identifier
Comment         <- "//" [^\n]*
NEWLINE         <- "\n"
INDENT          <- <increase in indentation level>
DEDENT          <- <decrease in indentation level>
EOF             <- <end of input>
```
