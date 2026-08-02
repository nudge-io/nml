# NML Error Index

Every diagnostic with a stable code has a section here, keyed `## NML0000`.
Codes are **stable from the first published release**: never renumbered,
never reused; a retired code keeps its section as a tombstone (see the
[stability policy](../../../docs/stability.md)). This index is bidirectionally guarded
by `just docs-test`: a code without a section — or a section without a code —
fails CI, and most examples below run through the real CLI.

**Sections are in ascending code order** — this is a lookup table, so a new
section goes beside its numeric neighbours rather than at the end (also
CI-enforced; the constants in `diagnostic.rs` carry the same rule at compile
time, and the ordering is what makes `nml explain --list` scannable).

Bands (allocation convenience, not API): 0001–0999 lex/parse ·
1000–1999 symbols & resolution · 2000–2999 schema loading & validation ·
3000–3999 values, money & durations · 4000–4999 packages & store ·
5000–5999 editor/LSP.

## NML0001

**Replaced syntax.** This syntax was removed by a language migration and has
a mechanical replacement — the fix is machine-applicable (editors offer it
as a quick-fix; the message names the exact rewrite). This is the
[stability policy](../../../docs/stability.md)'s "breaking changes ship with
fixers" commitment, as a diagnostic.

```nml check expect-error='[NML0001]'
oneof email by kind:
    "log" => emailLog
```

**Fix:** apply the suggestion (here: `->`).

**Migration ledger** (every rename this code has covered):

| Removed | Replacement | Since |
|---|---|---|
| `=>` (arm arrow) | `->` | RFC 0006 |
| `&&` (never valid — C-family habit) | `&` | RFC 0014 |
| `"30s"` (quoted duration in a `duration`-typed field) | the duration literal `30s` | RFC 0017 |
| `"3000"` (quoted number in a `number`-typed field) | the number literal `3000` | RFC 0017 follow-up |
| `"true"` (quoted bool in a `bool`-typed field) | the bool literal `true` | RFC 0017 follow-up |

The quoted-literal arms fire only when the de-quoted text parses as the
field's type — anything else stays the ordinary NML2008 type mismatch.
`$ENV.KEY` references are untouched (they resolve later); only source
literals migrate.

```nml check expect-error='[NML0001, NML0001]' schema=docs/errors/schemas/replaced-literals
svc Api:
    port = "3000"
    admin = "true"
```

Earlier pre-code migrations, for the record: the `!` positional marker
became `+` (RFC 0005 rev. 1), and the never-implemented angle-bracket
shorthand annotation was removed in favor of `+` (RFC 0005).

## NML0002

**Unexpected token.** The parser met something that fits none of the
alternatives valid at this position. The message lists what was expected
— concrete tokens and grammar classes — plus what was actually found;
when recovery tries several alternatives at one position, they merge into
a single "expected X or Y" report.

```nml check expect-error='[NML0002]'
service Api:
    port =
```

**Fix:** supply what the message asks for (here: a value after `=`).

## NML0003

**Unterminated string.** A string literal is missing its closing
delimiter. For multi-line `"""` strings the failure surfaces at
end-of-input — far from the cause — so a `note:` (and, in editors, a
related-information entry) points back at the opening delimiter.

```nml check expect-error='[NML0003]'
service Api:
    name = "abc
```

**Fix:** close the string (`"abc"`).

## NML0004

**Unexpected character.** A byte no NML token starts with. The character
is echoed with control characters escaped — file content can never
smuggle raw terminal escapes into your output.

```nml check expect-error='[NML0002, NML0002, NML0004]'
service Api:
    x = ^oops
```

**Fix:** remove the character, or quote it if it belongs in a string.

## NML0005

**Tab in indentation.** NML's offside rule measures indentation in
spaces; tabs would make column arithmetic depend on editor settings, so
they are rejected outright (the file still parses — the tab is treated
as whitespace).

```nml check expect-error='[NML0005]'
service Api:
	port = 8080
```

**Fix:** replace tabs with spaces (most editors have "convert
indentation to spaces").

## NML0006

**Inconsistent dedent — the offside rule.** Every block opens an
indentation column, and a line can only return to a column that is still
open. This line's indentation matches none of them; the message lists
the open columns, so the fix is a straight pick from that list. (NML
recovers by treating the line's column as a new level, so later lines
still parse.)

```nml check expect-error='[NML0002, NML0002, NML0002, NML0002, NML0006]'
service Api:
        port = 8080
    host = "0.0.0.0"
```

Here `port` opened column 8, so `host` at column 4 matches neither the
body (8) nor top level (0).

**Fix:** align the line with one of the open columns (here: 8 to stay in
the body, 0 for a new declaration).

## NML0007

**Nesting limit exceeded.** Block and type nesting are bounded (64
levels) — a deliberate defense: parsing is resilient on untrusted input,
and unbounded recursion would be a denial-of-service lever. Real
configurations sit nowhere near the bound; hitting it almost always
means generated or accidental structure. (Error *output* is bounded the
same way: at most 128 diagnostics, with an exact suppressed count when
clipping occurs.)

**Fix:** flatten the structure, or split the document.

## NML0008

**Set elements separated by a comma.** `set<a, b>` is the map habit —
set elements are *alternatives*, written `set<a | b>`. Machine-fixable:
the comma becomes `|`.

```nml check expect-error='[NML0002, NML0002, NML0008]'
model deploy:
    regions set<string, number>
```

**Fix:** apply the suggestion (`set<string | number>`).

## NML0009

**`map` is reserved.** `map` is held for a future map type; only `set`
takes type arguments today.

```nml check expect-error='[NML0009]'
model cache:
    entries map<string>
```

**Fix:** model the data another way (a nested model, or a list of keyed
items) until a map type ships.

## NML0010

**Unknown type constructor.** An identifier is used with type arguments
(`name<…>`), but only `set` is a constructor. Near-misses get a
machine-applicable did-you-mean.

```nml check expect-error='[NML0010]'
model deploy:
    regions sett<string>
```

**Fix:** apply the suggestion (`set`), or drop the angle brackets.

## NML0011

**Duplicate directive.** Each `#directive` key may appear once per field
— repeating one is a merge with no defined winner, so it is rejected.

```nml check expect-error='[NML0011]'
model server:
    rate number #live #live
```

**Fix:** delete the duplicate.

## NML0012

**Invalid string escape.** The escape is unknown, the string ends
mid-escape, or a `\u{…}` escape is malformed. Valid escapes: `\"` `\\`
`\n` `\t` `\r` `\u{…}` (1–6 hex digits naming a Unicode scalar, as in
Rust and Swift) — the message names the exact problem.

```nml check expect-error='[NML0012]'
service Api:
    x = "a\q"
```

```nml check expect-error='[NML0012]'
service Api:
    x = "\u{D800}"
```

**Fix:** use a valid escape, or double the backslash for a literal one
(`"a\\q"`). Surrogates (D800–DFFF) and code points above 10FFFF are not
scalars and cannot be written.

## NML0013

**Invalid number.** The literal is not a number the grammar parses —
most commonly a second decimal point, a trailing decimal point with no
fraction digits (`1299.`, machine-applicable remove-the-dot fix), or a
misplaced `_` digit separator. Separators are legal only **between two
digits** (`10_000`; never leading, trailing, doubled, or dot-adjacent —
one spelling per grouping, stricter than Rust), and a misplaced one gets
a machine-applicable strip-the-separators fix, provably value-preserving
(separators are spelling, never value).

```nml check expect-error='[NML0013]'
service Api:
    x = 1.2.3
```

```nml check expect-error='[NML0013]'
service Api:
    x = 1__000
```

**Fix:** write one decimal point (`1.23`), drop a trailing dot
(`1299.` → `1299`), apply the strip fix (`1__000` → `1000`), or quote it
if it is meant as a string.

## NML0014

**Number out of range.** NML numbers are exact decimals by design —
every value is stored with up to 34 significant digits in the IEEE
754-2019 decimal128 range, and a number that cannot be stored exactly
is an error, never a silently rounded float. This applies to integers
and decimals alike: `taxRate = 0.20` stores exactly `0.20`, and any
integer up to 34 digits (well past `u64`) parses exactly.

```nml check expect-error='[NML0014]'
service Api:
    x = 123456789012345678901234567890123456789
```

Three things can put a number outside the domain, and the message says
which: **too many significant digits** (the value cannot be stored
exactly), **too large** (beyond ~9.999×10^6144), or **too small** (a
nonzero value closer to zero than 10^-6176). The last two are about
magnitude, not digit count — `1` followed by 6145 zeros has a single
significant digit and is still out of range.

**Fix:** use a value with at most 34 significant digits *and* a
magnitude inside the exact range, or a string if it is an identifier
(account numbers usually are). The digit-count message carries the exact
count — trailing zeros beyond the budget
are dropped losslessly and do not trigger this error.

## NML0015

**Malformed variable reference.** A `$NS.key` reference needs a known
namespace, a dot, and a key. Unknown namespaces get a machine-applicable
did-you-mean over the valid sources.

```nml check expect-error='[NML0015]'
service Api:
    key = $ENVV.API_KEY
```

**Fix:** apply the suggestion (`$ENV.API_KEY`).

## NML0016

**Bare carriage return.** A CR with no following LF. NML line endings
are LF or CRLF (the source-character policy: *raw is transport, escaped
is content*); a bare CR is invisible in most editors and diff viewers,
so it is either file corruption or content smuggling — never intent.
The file still parses (the CR is treated as whitespace), and the fix is
machine-applicable: remove it.

The fence below is stored with LF endings and converted to lone-CR
("old Mac") endings by the docs harness before it runs, so this
example is executable, not illustrative:

```nml check eol=cr expect-error='[NML0002, NML0002, NML0016, NML0016]'
service Api:
    port = 8080
```

**Fix:** remove the stray CR (apply the suggestion), or re-save the
file with LF or CRLF line endings. For a literal CR *inside a string
value*, write `\r`.

## NML0017

**Raw control character.** A C0 control (other than tab and line
endings) or DEL appears raw in source — in a value, a comment, or
between tokens. Control characters are content, and content belongs in
escapes, where review can see it: a raw ESC in a value is a
terminal-injection primitive when that value is later printed, and a
raw NUL truncates C strings downstream. Tab is legal raw in string
content; only indentation restricts it ([NML0005](#nml0005)).

The escaped spelling is always available and is the fix:

```nml check
service Api:
    ansi_reset = "\u{1B}[0m"
```

**Fix:** replace the raw control character with its `\u{…}` escape.

## NML0018

**Invisible directionality character.** A bidirectional control
(U+202A–U+202E, U+2066–U+2069) or an interior U+FEFF can make source
*display* differently than it *parses* — the Trojan Source attack
(CVE-2021-42574). NML matches rustc's banned set. A leading U+FEFF is
accepted as a byte-order mark; everywhere else U+FEFF is this error.

Right-to-left *text* is unaffected: Hebrew and Arabic string values
need no bidi controls to render correctly. Only the explicit override
and isolate controls — the ones that can reorder what a reviewer sees —
are banned, and only in their raw form:

```nml check
service Api:
    rlo = "\u{202E}"
```

**Fix:** if the character is intentional content, write its `\u{…}`
escape so it is visible in review; otherwise delete it (it usually
arrives via copy-paste from rendered text).

## NML0019

**Content on a multi-line string's opening line.** The content of a
`"""` string begins on the line *after* the opening quotes — the same
rule Swift and Java text blocks enforce. Text on the opening line would
participate in the indentation-stripping computation, making the value
depend on where the content happens to sit; NML's dedent is computed
from transport shape alone, so this is closed as an error rather than
left as a trap.

```nml check expect-error='[NML0019]'
service Api:
    motd = """All systems operational.
        Subscribe for updates.
        """
```

**Fix:** move the content to the next line. For a short single-line
value, use an ordinary `"…"` string. (Whitespace alone after the
opening quotes is harmless and legal, as is the empty `""""""`.)

## NML0020

**Misaligned closing `"""`.** When the closing quotes stand on their own
line, they must align with the content's indentation. With alignment
enforced, the two ways a reader might understand dedent — "strip to the
closing delimiter" (Swift's model) and "strip the common indent" (NML's)
— *provably agree on every accepted document*, so neither can be
misread. The fix is machine-applicable: moving the delimiter is
value-preserving, because its line is trimmed either way.

```nml check expect-error='[NML0020]'
service Api:
    motd = """
        All systems operational.
    """
```

**Fix:** apply the suggestion — indent the closing quotes to the
content's column (here: 8). A closing delimiter on the last content
line has no alignment to check and stays legal.

## NML0021

**Fallback chain in a list position.** A fallback chain (`a | b`)
resolves to *one* value, but a list element's written form is also its
*identity* — set uniqueness and reload diffing key on it — and an
anonymous chain has no stable identity across environments. Elements are
therefore single values. (The `|` here is also easy to confuse with the
`|modifier` line syntax; this error names the actual mistake.)

```nml check expect-error='[NML0021]'
service Api:
    keys:
        - $ENV.A | $ENV.B
```

**Fix:** name the chain and reference it — `const PrimaryKey = $ENV.A |
$ENV.B`, then `keys = [PrimaryKey]` (the name is the element's identity;
the chain resolves at the definition). Write separate items (`- $ENV.A`
/ `- $ENV.B`) if you want *both* values, or use a property (`key =
$ENV.A | $ENV.B`) if you want one value with a fallback. Note: in the
dash spelling a bare name is a reference to a *declared item*, not a
`const` — use the inline spelling (`keys = [PrimaryKey]`) for `const`
references.

## NML1000

**Duplicate declaration.** Two top-level declarations share one name; names
are a single namespace so references stay unambiguous. The second
declaration is flagged; the first wins downstream.

```nml check expect-error='[NML1000]'
service Api:
    port = 8080

service Api:
    port = 9090
```

**Fix:** rename or merge one of the declarations.

## NML1001

**Unresolved reference.** A value references a name no declaration defines.
Comes with a did-you-mean when a declared name is close.

```nml check expect-error='[NML1001]'
const DefaultPort = 8080

service Api:
    port = DefaultPrt
```

**Fix:** apply the suggestion (`DefaultPort`), or declare the missing name.

## NML1002

**Const/template reference cycle.** `const`/`template` chains must resolve to
a value; a cycle never does.

```nml check expect-error='[NML1002, NML1002]'
const A = B
const B = A
```

**Fix:** break the cycle by giving one member a concrete value.

## NML2000

**Invalid enum value.** The value is not one of the enum's declared variants
(matching is exact; a near-miss gets a machine-applicable suggestion).

```nml check schema=docs/examples/errors expect-error='[NML2000]'
service Api:
    host = "0.0.0.0"
    port = 8080
    logLevel = "wran"
    apiKey = $ENV.API_KEY
```

**Fix:** apply the suggestion (`"warn"`), or use any declared variant.

## NML2001

**Unknown property.** The property is not defined by the governing model — a
warning by default (unknown data is skippable), an error under `--strict`.

```nml check schema=docs/examples/errors expect-output='[NML2001]'
service Api:
    host = "0.0.0.0"
    hots = "typo"
    port = 8080
    apiKey = $ENV.API_KEY
```

**Fix:** apply the suggestion, or add the field to the model.

## NML2002

**Unknown modifier.** The `|modifier` name is not in the configured modifier
set (project config or package profile). Surfaces wherever a modifier
vocabulary is configured; comes with a did-you-mean.

**Fix:** apply the suggestion (e.g. `|alow` → `|allow`), or register the
modifier.

## NML2003

**Unknown oneof discriminant.** The discriminator value matches no declared
arm of the `oneof`.

```nml check schema=docs/errors/schemas expect-error='[NML2003]'
email Notifier:
    kind = "postmrak"
```

**Fix:** apply the suggestion (`"postmark"`), or use any declared arm value.

## NML2004

**Unknown block keyword (strict).** Under `--strict`, a block keyword with no
model or `oneof` definition is an error instead of being silently skipped.

```nml check strict schema=docs/examples/errors expect-error='[NML2004]'
servce Api:
    port = 8080
```

**Fix:** apply the suggestion (`service`), or define the model.

## NML2005

**Unknown array keyword (strict).** The array's item keyword names no model
or `oneof`, and its items carry bodies that would go unvalidated.

```nml check strict schema=docs/examples/errors expect-error='[NML2005]'
[]widget Widgets:
    - first:
        size = 1
```

**Fix:** define the item model, or correct the keyword.

## NML2006

**Literal in a `secret` field.** `secret` fields hold *references*
(`$ENV.NAME`), never literal credential material — the file must never
contain the secret value (see the [stability policy](../../../docs/stability.md)'s
security notes and the README's secrets section).

```nml check schema=docs/examples/errors expect-error='[NML2006]'
service Api:
    host = "0.0.0.0"
    port = 8080
    apiKey = "sk-live-credentials-in-git"
```

**Fix:** use `$ENV.API_KEY` (fallbacks may chain to other references:
`$ENV.API_KEY | $ENV.API_KEY_DEV`), or declare the field `(secret | string)`
if literals are genuinely intended.

## NML2007

**Missing required field.** Fields are required unless marked `?` or given a
default; the instance omits one.

```nml check schema=docs/examples/errors expect-error='[NML2007]'
service Api:
    port = 8080
    apiKey = $ENV.API_KEY
```

**Fix:** supply the field (`host`), mark it optional, or give it a default.

## NML2008

**Type mismatch.** The value's type does not match the field's declared type.

```nml check schema=docs/examples/errors expect-error='[NML2008]'
service Api:
    host = "0.0.0.0"
    port = "eight thousand"
    apiKey = $ENV.API_KEY
```

**Fix:** supply a value of the declared type (`port number`).

## NML2009

**Duplicate definition.** The same model/enum/oneof name is defined more than
once across a schema set; the first definition wins downstream.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009]'
// a.model.nml and b.model.nml both define:
model widget:
    name string+
```

**Fix:** remove or rename one definition.

## NML2010

**Reserved type-constructor name.** `set` (live) and `map` (reserved) are
type constructors; a definition so named could never be referenced with
arguments.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2010]'
model set:
    x string?
```

**Fix:** rename the definition.

## NML2011

**Multiple positional fields.** A bare scalar list item supplies one value,
so a model may mark at most one field positional (`+`).

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2011]'
model twoPositional:
    a string+
    b string+
```

**Fix:** keep one `+`; the others become named properties.

## NML2012

**Oneof arm references an unknown model.** Every arm's target must be a
declared `model`.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2012]'
oneof thing by kind:
    "x" -> missingModel
```

**Fix:** declare the model, or correct the arm's target name.

## NML2013

**Inheritance cycle.** `is` chains must be acyclic.

```nml check expect-error='[NML2013, NML2013]'
model cycleA is cycleB:
    x string?
model cycleB is cycleA:
    y string?
```

**Fix:** break the cycle; extract shared fields into a trait both compose.

## NML2014

**Model-reference cycle (advisory).** Model fields reference each other in a
loop. Legal — recursive configs exist — but often an unintended
self-reference, so it warns.

```nml check expect-output='[NML2014, NML2014]'
model node:
    next otherNode?
model otherNode:
    back node?
```

**Fix:** if intended, ignore (it is only a warning); otherwise break the
loop.

## NML2015

**Duplicate discriminator value.** Each arm's value must be unique within
its `oneof` — dispatch would otherwise be ambiguous.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2015]'
oneof dupValue by kind:
    "x" -> widget
    "x" -> widget
```

**Fix:** give each arm a distinct value.

## NML2016

**Oneof name collision.** A `oneof` shares a name with a model or enum;
names are one namespace across all three definition kinds.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2016]'
oneof widget by kind:
    "w" -> widget
```

**Fix:** rename the union (or the colliding definition).

## NML2017

**Default discriminator matches no arm.** A declared default must name one
of the union's arm values.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2017]'
oneof withDefault by kind = "zzz":
    "a" -> widget
```

**Fix:** point the default at a declared arm value.

## NML2018

**Discriminator type is not an enum.** `by <field> as <type>` requires
`<type>` to be a declared enum.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2018]'
oneof badType by kind as notAnEnum:
    "a" -> widget
```

**Fix:** declare the enum, or drop the `as` clause.

## NML2019

**Enum-typed arms are not exhaustive.** With `as <enum>`, the arm values
must equal the enum's variants exactly — no missing variant, no arm outside
the enum. Both directions report this code.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2009, NML2019]'
enum letters:
    - "a"
    - "b"
oneof exhaustive by kind as letters:
    "a" -> widget
```

**Fix:** add the missing arm (or remove the extra one).

## NML2020

**Unknown `is` target.** Every `is` target must resolve to a declared
model or trait (RFC 0011). A typo'd target carries a machine-applicable
did-you-mean; in the editor it is a one-click fix.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2009, NML2020]'
trait auditable:
    auditedBy string?

model gadget is auditible:
    name string?
```

**Fix:** correct the name (`is auditable`), or declare the missing
model/trait.

## NML2021

**`is` target is not composable.** Only models and traits compose with
`is`; an enum or a `oneof` cannot be mixed in.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2009, NML2021]'
enum sizes:
    - "s"
    - "m"

model sized is sizes:
    n string?
```

**Fix:** reference the enum from a field (`size sizes`) instead of
composing it, or make the target a model/trait.

## NML2022

**A trait used as a field type.** Traits are composition-only (RFC 0011):
they bundle fields for `is`, and never describe a value — not directly,
in `[]`/`set<>`, in a union, behind a `|` modifier, or in `(K -> V)` arm
positions.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2009, NML2022]'
trait auditable:
    auditedBy string?

model holder:
    audit auditable?
```

**Fix:** mix the trait into the model (`model holder is auditable:`), or
declare a `model` if you need a nested value type.

## NML2023

**A `oneof` arm targets a trait.** Union variants are instantiated by
discriminator dispatch, so every arm must name an instantiable model —
a trait can never be selected.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009, NML2009, NML2023]'
trait auditable:
    auditedBy string?

oneof entry by kind:
    "a" -> auditable
```

**Fix:** point the arm at a model (one that may itself mix the trait in
with `is`).

## NML2024

**A trait instantiated.** A block (or array-declaration item) keyword
names a trait. Traits declare capability bundles, not block types, so
this is an error even in lenient mode — the schema *knows* the name, and
it is never a valid keyword.

```nml check schema=docs/errors/schemas expect-error='[NML2024]'
notifiable Alerts:
    channel = "ops"
```

**Fix:** instantiate a model that mixes the trait in with `is`, or
promote the trait to a `model` if it really is a standalone block type.

## NML2025

**A mixin listed twice.** The same model/trait appears more than once in
one `is` clause. The merge is idempotent — nothing breaks — but the
duplicate is noise, usually copy-paste residue or a rename that collapsed
two parents into one. (Transitive "diamonds" — `x is a, b` where `b`
itself mixes in `a` — are fine and never flagged; that composition is the
point of mixins.)

```nml check expect-output='[NML2025]'
trait monitored:
    timeout duration = 5s

model endpoint is monitored, monitored:
    url string+
```

**Fix:** delete the duplicate entry.

## NML2026

**In-file definitions under a closed binding.** This file is validated by
a schema **package binding** — a tool's published, composed schema set —
and that set is the entire vocabulary, so `model`/`trait`/`enum`/`oneof`
definitions authored in the file have no effect: they type nothing, and a
keyword they would introduce still fails strict validation as unknown. A
warning in lenient validation; an **error** under strict. (Outside package
bindings — plain `nml check` — in-file definitions are first-class and
type the file's own instances.)

```nml fragment
// In a file bound by a tool's schema package:
model smuggled:      // NML2026 — has no effect here
    x string?

smuggled Foo:        // still an unknown keyword under strict
    x = "boo"
```

**Fix:** remove the definitions, or move them into the tool's schema
package where they become real vocabulary.

## NML2027

**Duplicate enum variant.** The same variant appears more than once (both
authored forms — `- "a"` and `- a` — name one variant). Harmless at
runtime, definitely unintended.

```nml check expect-output='[NML2027]'
enum level:
    - "info"
    - info
```

**Fix:** delete the duplicate.

## NML2028

**Empty enum.** The enum declares no variants, so no value can ever
satisfy a field it types — and a `oneof … as` clause can never cover it.
A warning (an enum is transiently empty while you type it); make it a
hard gate with `--strict` in CI.

```nml check expect-output='[NML2028]'
enum pending:
```

**Fix:** add variants, or remove the enum until it has some.

## NML2029

**Retired** (RFC 0017 — durations became literals). This code guarded the
duration *format* back when a duration was a quoted string only schema
validation could judge. The parser now guarantees the format, so the
check cannot fire: a malformed literal surfaces at decode as
[NML3004](#nml3004) (unknown unit), [NML3005](#nml3005) (fractional
magnitude), [NML3006](#nml3006) (out of domain), [NML3007](#nml3007)
(duplicate unit), or [NML3008](#nml3008) (malformed compound); a quoted duration in
a duration-typed field is the [NML0001](#nml0001) migration; and any
other value there is the ordinary [NML2008](#nml2008) type mismatch.
Codes are never renumbered or reused, so this section remains as the
tombstone.

## NML2030

**Duplicate set element.** `set<T>` elements are unique by definition;
identity is value-level (the same value admitted via different union arms
is still one element). The second occurrence is flagged.

```nml check expect-error='[NML2030]'
model deploy:
    regions set<string>

deploy Prod:
    regions = ["us-east", "us-east"]
```

**Fix:** remove the duplicate element.

## NML2031

**Non-arm entry in an arms body.** A `(K -> V)`-typed field's body holds
only routing arms (`@selector -> target`, `else -> target`); a plain
property has no meaning there.

```nml check expect-error='[NML2031]'
model service:
    landing (role -> string)?

service Api:
    landing:
        theme = "dark"
```

**Fix:** write arms (`@role/admin -> "ops"`), or move the property out of
the arms body.

## NML2032

**No union variant matches.** The value matches none of the union type's
variants.

```nml check expect-error='[NML2032]'
model service:
    contact (string | []string)?

service Api:
    contact = 7
```

**Fix:** supply one of the listed shapes.

## NML2033

**Type composition with no instance form.** RFC 0007 §4.3: an arm set
(`(K -> V)`) describes a field's *body*, so it cannot appear where a body
can never hold arms — as an array/set element, an arm-set key or target,
or a modifier's declared type. A union may carry at most one arm-set
variant (body shape selects the variant; a second arm-set variant would be
unreachable).

```nml check expect-error='[NML2033]'
model service:
    |landing (role -> string)?
```

**Fix:** restructure the type — e.g. declare the routing as a plain field
(`landing (role -> string)?`), which modifiers can then reference.

## NML2034

**Misplaced field definition.** `name type` field definitions belong in
`model`/`trait` declarations; in other blocks they have no meaning.

```nml check expect-error='[NML2034]'
model widget:
    name string?

service Api:
    port number
```

**Fix:** move the definition into a model, or write an instance property
(`port = 8080`).

## NML2035

**Routing arms in a schema declaration.** A declaration carries the
`(K -> V)` *type*; the arms themselves belong in instance blocks.

```nml check expect-error='[NML2035]'
model service:
    @role/admin -> "ops"
```

**Fix:** declare `landing (role -> string)?` here and write the arms in
the instance.

## NML2036

**Duplicate arm.** An arm set repeats a selector — a second `else`, or the
same arm key twice. Arms match first-to-last, so the duplicate could never
apply.

```nml check expect-error='[NML2036]'
model service:
    landing (role -> string)?

service Api:
    landing:
        else -> "status"
        else -> "ops"
```

**Fix:** remove the duplicate arm.

## NML2037

**Unreachable arm.** An arm after `else` can never match — `else` is the
catch-all, so it must be the final arm.

```nml check expect-error='[NML2037]'
model service:
    landing (role -> string)?

service Api:
    landing:
        else -> "status"
        @role/admin -> "ops"
```

**Fix:** move `else` to the end.

## NML2038

**Arm key mismatch.** The arm's selector does not conform to the arm set's
declared key type.

```nml check expect-error='[NML2038]'
model service:
    landing (string -> string)?

service Api:
    landing:
        @role/admin -> "ops"
```

**Fix:** use a selector of the declared key type (e.g. `@role/…` for a
`role`-keyed arm set, a string key for a `string`-keyed one).

## NML2039

**Arm target mismatch.** A string-literal target (`-> "value"`) requires a
scalar-capable target type; a model-typed arm set needs a declared name or
an inline block (`-> Name:`). This value is neither.

```nml check expect-error='[NML2039]'
model page:
    path string?

model service:
    landing (role -> page)?

service Api:
    landing:
        else -> "status"
```

**Fix:** point the arm at a declared instance (`-> StatusPage`) or write an
inline block (`-> StatusPage:`).

## NML2040

**Arms where fields are expected.** A routing arm inside a model-typed
body — arms belong under a field typed `(K -> V)`.

```nml check expect-error='[NML2040]'
model service:
    host string?

service Api:
    else -> "status"
```

**Fix:** declare an arm-typed field and put the arms in its block.

## NML2041

**Missing discriminator.** The `oneof` instance omits its discriminator
and the union declares no default arm.

```nml check expect-error='[NML2041]'
model a:
    x string?
model b:
    y string?

oneof entry by kind:
    "a" -> a
    "b" -> b

entry E:
    x = "1"
```

**Fix:** set the discriminator, or give the union a default arm
(`by kind = "a"`).

## NML2042

**Invalid discriminator.** The discriminator's value must be a string
naming an arm.

```nml check expect-error='[NML2042]'
model a:
    x string?

oneof entry by kind:
    "a" -> a

entry E:
    kind = 5
```

**Fix:** use one of the declared arm strings.

## NML2043

**Shorthand on a union-typed list.** A bare scalar item can't select a
union variant — the variant is undecidable from one token.

```nml check expect-error='[NML2043]'
model run:
    cmd string?
model wait:
    seconds number?

oneof step by kind:
    "run" -> run
    "wait" -> wait

model pipeline:
    steps []step?

pipeline P:
    steps:
        - "make test"
```

**Fix:** write the item in block form and select the variant explicitly.

## NML2044

**Validation truncated.** Nesting exceeded the maximum validation depth;
deeper entries were not checked (advisory). Almost always a sign of
generated or accidental extreme nesting.

**Fix:** flatten the structure, or split the document.

## NML2045

**Role written as a string.** `role`-typed fields hold *references*
(`@name`), not strings. Machine-fixable — the suggestion removes the
quotes and adds the `@` when missing.

```nml check expect-output='[NML2045]'
model resource:
    owner role?

resource Home:
    owner = "admin"
```

**Fix:** apply the suggestion (`@admin`).

## NML2046

**User reference in an access rule.** `@user/…` references identify
members; access-control rules (`|allow`/`|deny`) take roles. Surfaced
where a package configures membership semantics (RFC 0030).

**Fix:** put the user in the role's members list and allow the role.

## NML2047

**Built-in access level in a members list.** `@public`-style levels are
access semantics, not members. Surfaced under package membership
semantics.

**Fix:** remove it; use it in `|allow` instead.

## NML2048

**Membership cycle.** Role/plan membership references form a cycle
(advisory). Surfaced under package membership semantics.

**Fix:** break the cycle at its least meaningful edge.

## NML2049

**Dropped item key.** A bare scalar list item supplies one value, but the
element model declares no positional (`+`) field to receive it — the key
has nowhere to go.

```nml check expect-error='[NML2049]'
model step:
    run string?

model job:
    steps []step

job Nightly:
    steps:
        - "make test"
```

**Fix:** mark one field positional (`run string?+`), or write the item in
block form.

## NML2050

**Arm-shorthand mismatch.** A bare scalar item fills an arm-set (`(K -> V)`)
shorthand field through the canonical `else ->` embedding — so the value
must be a name, a string literal, or an inline block (`-> Name:`). This
value is neither, and no arm can be synthesized from it.

```nml check expect-error='[NML2050]'
model page:
    landing (role -> string)+

[]page pages:
    - 42
```

**Fix:** supply a name or a string (it becomes the `else ->` arm's
target), write an inline block (`-> Name:`), or write the item in block form
with explicit arms.

## NML2051

**Unknown union variant.** An `as <Variant>` annotation (RFC 0015) must name
one of the union's variants. This one names a type the union does not include;
a did-you-mean points at the closest match.

```nml check expect-error='[NML2051]'
model alpha:
    a string?
model beta:
    b string?
model host:
    slot (alpha | beta)?

host H:
    slot as gamma:
        a = "x"
```

**Fix:** annotate with one of the union's variants (`slot as alpha:` /
`slot as beta:`), or accept the completion / did-you-mean suggestion.

## NML2052

**Ambiguous union instance.** A same-class union instance carries no
`as <Variant>` annotation and its body shape cannot choose between two or more
model variants. NML is fail-closed here: rather than silently guessing the
first variant, it asks you to state the type (RFC 0015 D2).

```nml check expect-error='[NML2052]'
model alpha:
    a string?
model beta:
    b string?
model host:
    slot (alpha | beta)?

host H:
    slot:
        a = "x"
```

**Fix:** state the variant with `as` — e.g. `slot as alpha:`.

## NML2053

**Stray type annotation.** An `as <Variant>` annotation (RFC 0015) selects a
union variant. This field is not a union, so there is no variant to choose and
the annotation has no effect — flagged rather than silently ignored.

```nml check expect-error='[NML2053]'
model inner:
    x string?
model host:
    slot inner?

host H:
    slot as other:
        x = "v"
```

**Fix:** drop the annotation (`slot:`), or change the field's type to a union
if you meant to choose between variants.

## NML2054

**Shadowed discriminator (advisory).** A `oneof` arm's model declares a
field named like the union's discriminator. An instance's property of that
name is always read as the discriminator, so the field itself can never be
set — almost always an authoring mistake, so it warns.

```nml check expect-output='[NML2054]'
model logEntry:
    kind string?
oneof record by kind:
    "log" -> logEntry
```

**Fix:** rename the model field (or the discriminator) so the two don't
collide; a modifier-form field (`|kind`) is distinct authoring and does not
shadow.


## NML2055

**Dropped item body.** A list item carries a body, but the element type
is a scalar (or a union/collection of scalars) with no fields to fill —
every entry in the body would be silently discarded. The body-side
mirror of [NML2049](#nml2049)'s dropped key: content with nowhere to go
is an error, never leniency, because `nml check` passing is the promise
that nothing in the file is ignored.

```nml check expect-error='[NML2055]'
model host:
    tags []string?

host H:
    tags:
        - foo:
            note = "this body has nowhere to go"
```

**Fix:** write the item as a scalar (`- "foo"`), or — if the entries
are real configuration — give the elements a model type that declares
those fields.

## NML2057

**Facet violation.** A `number` or `duration` value falls outside a
facet its schema declares (RFC 0018): `min`/`max` (inclusive),
`exclusiveMin`/`exclusiveMax`, or `multipleOf`.

```nml check expect-error='[NML2057]'
model server:
    port number(min = 1, max = 65535)

server Web:
    port = 70000
```

The message names the field, its value, and the facet as authored:
`'port' is 70000, above the schema's max = 65535`.

Enforcement is **exact**: number bounds compare through the RFC 0016
decimal core, and `multipleOf` is decided by exact decimal
divisibility — so `0.3` IS a multiple of `0.1` here (binary-float
validators famously say otherwise), and boundary values behave like
the schema reads. Duration bounds compare **semantically** in
nanoseconds (RFC 0017): `1000ms` satisfies `min = 1s`, and
`multipleOf` is unit-blind divisibility — `1500ms` is a multiple of
`250ms` but not of `1s`:

```nml check expect-error='[NML2057, NML2057]'
model job:
    interval duration(min = 1s, multipleOf = 250ms)

job Sync:
    interval = 900ms
```

Values are checked after the type check, element-wise for
collections, and field defaults are held to the same rule.

**Fix:** change the value to satisfy the constraint, or change the
schema if the constraint is wrong. Nothing is ever clamped or rounded
for you.

## NML2058

**Invalid facet declaration.** The schema itself misuses facets
(RFC 0018): facets on a type that is neither `number` nor `duration`,
a facet value in the wrong domain (a unitless bound on a `duration`
field, a duration bound on a `number` field), an unknown or duplicate
facet key, `min`/`exclusiveMin` (or `max`/`exclusiveMax`) together, an
unsatisfiable range (`min = 2, max = 1` — judged semantically for
durations, so `exclusiveMin = 1000ms, max = 1s` is empty — or an
exclusive bound meeting its counterpart), or `multipleOf` that is not
positive (`0s` included). (A default violating its own facets reports
as NML2057 — it is a value breaking a constraint, found where values
are checked.)

```nml check expect-error='[NML2058]'
model m:
    name string(min = 1)
```

The message names the field and the rule: ``'name': facets attach only
to `number` and `duration` — `string` cannot carry them``.

A cross-domain facet value is the same code — the field's type picks
the domain, and the bound must be written in it:

```nml check expect-error='[NML2058]'
model job:
    timeout duration(min = 5)
```

The message teaches the literal shape: ``'timeout': `duration` facets
take duration literals (`min = 5s`, `min = 5ms`, ...) — `5` has no
unit``.

**Fix:** move range constraints to `number` or `duration` fields
(duration bounds are duration literals: `min = 5s`); string/collection
length constraints are deliberately not spelled with these keys.

## NML3000

**Invalid money literal.** The amount or its fractional part is not a
number (money is exact minor units, never floats).

```nml check expect-error='[NML3000]'
product Widget:
    price = 1..2 USD
```

**Fix:** write a plain decimal amount (`1.2 USD`).

## NML3001

**Unknown currency code.** The trailing code is not in the ISO 4217 table;
near-misses get a machine-applicable suggestion.

```nml check expect-error='[NML3001]'
product Widget:
    price = 19.99 USE
```

**Fix:** apply the suggestion (`USD`), or use any ISO 4217 code.

## NML3002

**Money precision exceeded.** More fractional digits than the currency's
ISO 4217 minor unit allows — the value could not be stored exactly, and
money is never rounded silently.

```nml check expect-error='[NML3002]'
product Widget:
    price = 19.999 USD
```

**Fix:** use the currency's precision (`19.99 USD`; JPY takes none:
`1999 JPY`).

## NML3003

**Money amount out of range.** The amount, scaled to minor units, exceeds
`i64` — exactness is the design, so overflow is an error, never a float.

```nml check expect-error='[NML3003]'
product Widget:
    price = 922337203685477581 USD
```

**Fix:** use a representable amount (the bound is ~92 quadrillion cents).

## NML3004

**Unknown unit.** A number's trailing identifier is neither a currency
code (exactly 3 uppercase letters, e.g. `USD`) nor a duration unit (`h`,
`m`, `s`, `ms`, `us`, `ns` — RFC 0017; ASCII `us`, since `µ` is not
source-legal). Case is meaningful: `30S` is a rejection with
a fix, never a case-fold, so one spelling per value holds and `M`/`m`
stays unambiguous forever. Near-miss units get a machine-applicable
suggestion on the suffix itself.

```nml check expect-error='[NML3004]'
service Api:
    requestTimeout = 30S
```

**Fix:** apply the suggestion (`30s`), or write one of `h`, `m`, `s`,
`ms`, `us`, `ns`.

## NML3005

**Fractional duration magnitude.** A duration magnitude is a whole
number — `30.5s` is rejected rather than rounded (the same
error-over-guessing rule exact numbers follow). When the value has an
exact whole-unit spelling, the fix respells it at the authored
granularity as a compound literal (`30.5s` → `30s500ms`, `1.5h` →
`1h30m`, `0.5ms` → `500us`); only past the domain's `ns` resolution
floor is there no fix. In a compound literal the fix is withheld —
replacing the whole literal with one component's respelling would drop
its siblings.

```nml check expect-error='[NML3005]'
service Api:
    requestTimeout = 30.5s
```

**Fix:** apply the suggestion (`30s500ms`), or pick the intended whole
magnitude.

## NML3006

**Duration out of domain.** Durations are unsigned and bounded: the
total must not exceed `std::time::Duration::MAX` (about 5.8 × 10¹¹
years), so every parsed duration converts to a runtime duration
infallibly — the same reject-at-decode posture money takes for amounts
beyond `i64` minor units ([NML3003](#nml3003)). Negative durations do
not exist (elapsed time has no sign in configuration; money differs
deliberately — refunds are real).

```nml check expect-error='[NML3006]'
service Api:
    requestTimeout = -30s
```

**Fix:** use a non-negative magnitude within the unit's stated maximum.

## NML3007

**Duplicate duration unit in a compound literal.** The same unit appears
more than once in one duration literal (`1h2h`). Unlike coercion from
machine-emitted strings, authored source is diagnosed rather than silently
merged — the fix replaces the literal with the merged canonical form (`3h`).

```nml check expect-error='[NML3007]'
service Api:
    requestTimeout = 1h2h
```

**Fix:** apply the suggestion (`3h`), or spell the intended components once each.

## NML3008

**Malformed compound duration.** A magnitude in a compound literal is not
followed by a unit suffix (`1h30`, `5m2`). Each component must be an
integer magnitude immediately followed by a unit (`h`, `m`, `s`, `ms`, `us`,
or `ns`).

```nml check expect-error='[NML3008]'
service Api:
    requestTimeout = 1h30
```

**Fix:** add the missing unit suffix, or remove the dangling magnitude.

## NML4000

**Fully shadowed validator.** A package validator binding's globs can
never match first — earlier bindings claim every file it would. Dead
configuration in the manifest (RFC 0030 meta-validation).

**Fix:** reorder the bindings, or remove the dead one.

## NML5000

**Unknown directive.** *(Editor/package surface.)* `#name` is not in the
covering schema package's directive vocabulary. Comes with a sigil-inclusive
did-you-mean (`#lvie` → `#live`).

**Fix:** apply the suggestion, or declare the directive in the package's
`[]directive` vocabulary.

## NML5001

**Directive arity mismatch.** *(Editor/package surface.)* The directive takes
no argument but was given one, or requires one and lacks it — per its
declared `arg` kind.

**Fix:** match the declaration (`#key("host")` vs `#live`).

## NML5002

**Contradictory directives.** *(Editor/package surface.)* `#live` and
`#restart` on the same field contradict — a field has one reload class.

**Fix:** keep exactly one.

## NML5003

**Undeclared sibling schema (advisory).** *(Editor/package surface.)* A
`.model.nml` file sits beside a package's sources but is not declared in the
manifest's `[]schema` list, so it does not participate in validation.

**Fix:** add a `[]schema` entry — or move the file if it is not meant to be
part of the package.

## NML5004

**Unknown template namespace.** *(Editor/project surface.)* A
`{{namespace.key}}` expression uses a namespace the project does not
configure (`templateNamespaces` in `nml-project.nml`). Comes with a
did-you-mean.

**Fix:** apply the suggestion, or add the namespace to the project config.
