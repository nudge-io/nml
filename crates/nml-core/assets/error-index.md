# NML Error Index

Every diagnostic with a stable code has a section here, keyed `## NML0000`.
Codes are **stable from the first published release**: never renumbered,
never reused; a retired code keeps its section as a tombstone (see the
[stability policy](../../../docs/stability.md)). This index is bidirectionally guarded
by `just gate-docs`: a code without a section — or a section without a code —
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

The quoted-literal arms accept the type's **coercion grammar** — the
same grammar `$ENV` values use at runtime — so every spelling that ever
worked through string coercion gets the migration fix, including ones
the literal grammar rejects (`"1e-6"`, `"1.5h"`). The suggestion is
always the **canonical literal** (`0.000001`, `1h30m`), which is valid
source even when the quoted spelling was not; the "(drop the quotes)"
hint appears only when de-quoting alone IS the fix. Anything outside
the coercion grammar stays the ordinary NML2008 type mismatch.
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
alternatives valid at this position.

The message lists what was expected — concrete tokens and grammar
classes — plus what was actually found; when recovery tries several
alternatives at one position, they merge into a single "expected X or
Y" report. A modifier block (`|deny:`) holds list items only, and an
array body (`[]validator validators:`) holds list items, properties,
modifiers and shared properties: any other line inside them (a `.shared`
property in a modifier block; a `key:` block, a field definition or a
routing arm dedented to an array body's item column) is reported here,
naming the line's kind, never silently dropped — `nml fmt` refuses the
file rather than writing it back without the line.

```nml check expect-error='[NML0002]'
service Api:
    port =
```

A fallback chain is one line: a `|` that ends a line has no arm and is
reported once, at the pipe (`expected a value after `|`, found a line
break`); the next line is the next entry, never the arm:

```nml check expect-error='[NML0002]'
service Api:
    host = $ENV.HOST |
    port = 3000
```

A `key:` block at a list body's item column — the shape a block takes
when pasted at the indentation a remedy printed it with:

```nml check expect-error='[NML0002]'
[]validator validators:
    - a:
        files:
            - "x/**"
    stray:
        allowRefs:
            - "y"
```

**Fix:** supply what the message asks for (here: a value after `=`), or
indent the block under the item it belongs to.

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

**Nesting limit exceeded.** Block, value and type nesting are bounded (64
levels), and so is a fallback chain (64 arms: `a | b | c` is read as `a`,
else `b | c`, so each arm is a level) — a deliberate defense: parsing is
resilient on untrusted input, and unbounded recursion would be a
denial-of-service lever. Real
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

A CR in token position is reported without a machine fix: on a
CR-terminated ("old Mac") file every CR is a line ending, and deleting
it would glue the lines together. Inside a string literal the CR is
content, and the machine fix is the `\r` escape (value-preserving).

That in-string fix is offered only where the escape provably leaves
the decoded value byte-identical — a CR in a multiline string's blank
edge line, in a blank line whose blankness holds the min-indent up, in
the opening line's dropped padding, or glued to a preceding backslash
is not string *content*, and the diagnostic stands there without a fix
(hand-writing the escape in those places would change the value; and
mind a preceding backslash when removing the character by hand — the
deletion changes what the backslash escapes, so re-check the value).
Very large string tokens (past 64 KiB) refuse machine repair entirely
— the judgment is never run, fail-closed — so the diagnostic stands
and the escape is written by hand.

The fence below is stored with LF endings and converted to lone-CR
("old Mac") endings by the docs harness before it runs, so this
example is executable, not illustrative:

```nml check eol=cr expect-error='[NML0002, NML0002, NML0016, NML0016]'
service Api:
    port = 8080
```

**Fix:** re-save the file with LF or CRLF line endings, or delete a
truly stray mid-line CR by hand. For a literal CR *inside a string
value*, write `\r` (the suggested fix).

## NML0017

**Raw control character.** Any Unicode control character (general
category Cc — C0, DEL, and the C1 range U+0080–U+009F), other than tab
and line endings, appears raw in source — in a value, a comment, or
between tokens; write it as its `\u{…}` escape.

Control characters are content, and content belongs in escapes, where
review can see it: a raw ESC in a value is a terminal-injection
primitive when that value is later printed, C1's CSI (U+009B) is the
same primitive in a single byte, and a raw NUL truncates C strings
downstream. NEL (U+0085) is a Unicode mandatory line break some
renderers honor — the error's hint offers `\n` and the mojibake `…`
beside the escape, since a pasted NEL usually meant a line break.
(Raw C1 inside valid
UTF-8 most often marks Windows-1252 double-decoding, so this error
catches real corruption too.) Tab is legal raw in string content; only
indentation restricts it ([NML0005](#nml0005)).

The escaped spelling is always available and is the fix:

```nml check
service Api:
    ansi_reset = "\u{1B}[0m"
```

**Fix:** replace the raw control character with its `\u{…}` escape.
Inside a string literal the repair is machine-applicable: for most
controls the escape is the one value-preserving reading, so `nml fix`
applies it (and the editor offers it) — offered only where the escape
provably leaves the decoded value byte-identical (a character in a
multiline string's blank edge line, a geometry-bearing blank line, the
opening line's dropped padding, or one glued to a preceding backslash
is not value content; the diagnostic stands there without a repair).
Very large string tokens (past 64 KiB) refuse machine repair entirely
(fail-closed — the value-preservation judgment is never run); the
diagnostic stands and the escape is written by hand.
Where the byte is genuinely
ambiguous the alternatives are enumerated instead and a human picks —
never auto-applied: NEL offers `\n` (a line break was meant), `\u{85}`
(keep the byte), or `…` (the Windows-1252 reading — 0x85 is the
ellipsis, the classic double-decode artifact); the other C1 bytes with
CP-1252 meanings offer their escape or their mojibake repair (`\u{93}`
or `“`). In token position there is no machine repair — the character
is structure there, and any rewrite would be a guess.

## NML0018

**Invisible steering character.** A character that can make source
*display* differently than it *parses* — or carry text the reader
cannot see: an explicit bidirectional control (U+202A–U+202E,
U+2066–U+2069 — the Trojan Source attack, CVE-2021-42574), an interior
U+FEFF, a U+2028/U+2029 line/paragraph separator, or a character from
the Unicode tag block (U+E0000–U+E007F, 128 code points); write it as
its `\u{…}` escape.

The separators are the only mandatory line breaks (UAX #14) that are
not control characters — an editor honoring them displays one authored
line as two. The bidi set matches rustc's Trojan-Source lints; with the
separators the whole set is a strict superset of rustc's, per the
display-vs-parse guidance of UTS #55 (Unicode Source Code Handling). A
leading U+FEFF is accepted as a byte-order mark; everywhere else U+FEFF
is this error.

The tag block is a deprecated invisible mirror of ASCII: a raw tag
sequence hides a full ASCII payload inside what displays as ordinary
text — a smuggling channel through human review and into any system
that echoes the value onward. Its one modern legitimate use, emoji tag
sequences (the Scotland flag is the black-flag base U+1F3F4 plus tag
letters spelling `gbsct`), is content, and content takes escapes:
`"\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}"`.

Right-to-left *text* is unaffected: Hebrew and Arabic string values
need no bidi controls to render correctly, and the implicit bidi marks
(LRM, RLM, ALM) stay legal — they are ordinary RTL content that
reorders only neighboring weak characters, never tokens. Only the
explicit override and isolate controls — the ones that can reorder what
a reviewer sees — and the separators are banned, and only in their raw
form:

```nml check
service Api:
    rlo = "\u{202E}"
```

**Fix:** if the character is intentional content, write its `\u{…}`
escape so it is visible in review; for U+2028/U+2029 a `\n` line break
is usually what was meant; otherwise delete it (it usually arrives via
copy-paste from rendered text). Inside a string literal these
alternatives are machine-offered — U+2028/U+2029 as `\n` or the
escape; a bidi control, interior U+FEFF, or tag character as *remove*
or the escape — and a human picks: intent is genuinely open, so
nothing auto-applies. The *remove* option is offered only where
deletion provably removes just that character; where it would disturb
the string's structure (a line flipping blank into the edge trim, the
indentation computation moving, quote runs merging into a delimiter)
the escape is offered alone — and, being the one sound reading there,
it auto-applies. In token position there is no machine repair. As with
[NML0017](#nml0017), repairs are fail-closed: a very large string token
refuses machine repair entirely and the diagnostic stands, while a
value that defeats the deletion judgment's sentinel machinery merely
loses the *remove* arm — the escape stands alone and auto-applies.

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

With no closing delimiter there is nothing to align: an unterminated
string reports only [NML0003](#nml0003) — the file's trailing blank
line is never treated as the closing quotes.

```nml check expect-error='[NML0003]'
service Api:
    motd = """
        All systems operational.

```

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

## NML0022

**Source too large.** A single NML source is parsed only up to 4 GiB
(4,294,967,295 bytes): token positions are 32-bit, the same ceiling the
syntax tree's own text offsets have. Past it nothing is lexed — the tree
is empty and this is its one finding — rather than a truncated or
partially indexed parse. The CLI's per-input caps (a 16 MiB check
target, a 256 KiB manifest, a 4 MiB declared source) refuse long before
this bound; it exists so that the parser itself never panics on any
input, whatever the caller's policy. No executed example can show it:
no front end lets a 4 GiB source reach the parser (the CLI refuses a
check target at 16 MiB, the editor a frame at 256 MiB), so the bound is
pinned by the lexer's own test rather than by a transcript.

**Fix:** split the document; a configuration this large is generated
structure, not a hand-written file.

## NML1000

**Duplicate declaration.** Two top-level declarations share one name —
a block, an array, a `const`, a `template` or a `oneof`, whatever their
keywords (`model api` beside `service api` is this error): names are a
single namespace so references stay unambiguous. The second declaration
is flagged at its name, with the first as a `note:` (`relatedInformation`
in the editor). The rule runs beside every parse, so the text does not
parse: `check` and `validate` stop at it as at any parse finding, `fmt`
writes nothing, and an embedder's `nml_core::parse` refuses the text. A
declaration the parser could not name (a stray token where one starts)
is no declaration: the parse finding stands alone, never a `duplicate
declaration ''` beside it.

```nml check expect-error='[NML1000]'
service Api:
    port = 8080

service Api:
    port = 9090
```

```text transcript=tests/fixtures/dup-names
$ nml fmt dupdecl.nml
dupdecl.nml:4:9: error[NML1000]: duplicate declaration 'Api' — a file declares each name once
dupdecl.nml:1:9: note: 'Api' first declared here
for more information, run: nml explain NML1000
error: 1 parse error(s)
```

A manifest's `package` block and its `[]schema`, `[]validator` and
`[]directive` arrays are declarations too: a second `package demo:` is
this error, refused where the manifest is parsed (the universe
closed-denied under NML2088). A second `[]validator` array under a
DIFFERENT name is the loader's manifest-shape rule instead — one slot
per keyword, NML2094 — under NML2088 as its cause. Across the sources
of one schema set, the
same kind of definition twice is the loader's NML2009; within one body,
the same entry twice is NML2093.

**Fix:** rename or merge one of the declarations — nothing can choose
for you.

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
(matching is exact; a near-miss gets a machine-applicable suggestion). The
package loader's own backstop for a `[]directive` entry's `arg` kind
reports under this code too, behind the meta-schema's finding.

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
default; the instance omits one. The package loader's own backstop for a
manifest's required entries (`version`, `formatVersion`, a `[]schema`
entry's `file`, a `[]directive` entry's `arg` and `doc`) reports under
this code too — the meta-schema's finding comes first, so a manifest
meets it there (NML2088 at the universe).

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

Under layer composition (RFC 0019 E16), a non-string discriminator
entry in ANY layer is reported on the composed view at its author's
span: stripping is by name, so such entries never compose over each
other — they pass through beside the canonical entry, and the layer's
other fields still compose. At the NML2054 shape (an arm field named
like the discriminator — refused at schema load) the composition still
draws this error rather than an NML2085 discard: the union field never
sees the entry.

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
one of the union's variants, and this one names a type the union does not
include.

A did-you-mean points at the closest match; a name that is a list
variant's element gets the honest form instead (list variants are
selected by shape, never named). Under layer composition
(RFC 0019) a bogus name never switches the established variant — the
compose engine treats it as un-annotated (fail-safe) and reports this
error itself, because the composed body carries the *established*
annotation and the validator would otherwise never see the authored one.

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
model variants.

NML is fail-closed here: rather than silently guessing the
first variant, it asks you to state the type (RFC 0015 D2). Layer
composition (RFC 0019) honors the same discipline: an ambiguous body is
composed model-less and left un-annotated — never guessed, never
stamped with a synthesized annotation — so this error fires on the
composed view exactly as it would on the raw one.

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

**Shadowed discriminator.** A `oneof` arm carries a plain field named
like the union's discriminator. An instance's property of that name is
always read as the discriminator, so the field can never be set:
required, it makes every instance unsatisfiable (a missing-field error
on a property the instance states — and stating it twice is NML2093);
optional, it is a declaration nothing can fill. An error at schema load
— a schema whose instances the language calls ill-formed does not load
— located at the field in the file that declares it, with the union's
declaration as a `note:`. The one spelling that stays is the seal: an
OPTIONAL `#sealed` field of that name is how an arm forbids switching
(RFC 0019), and being unsettable is its point. A modifier-form field
(`|kind`) is distinct authoring and never shadows.

```nml check expect-error='[NML2054]'
model logEntry:
    kind string?
oneof record by kind:
    "log" -> logEntry
```

The row carries the field's deletion as a machine-applicable fix — no
instance can have set the field, so nothing depends on it — which
`nml fix` applies (a comment above the field stays) and the editor
offers as a quick fix on the schema buffer:

```text transcript=tests/fixtures/shadowed-discriminator/own
$ nml check --root . shadow.model.nml
shadow.model.nml:3:5: error[NML2054]: oneof 'record' arm "log": model 'logEntry' declares a field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so the field can never be set; delete it (to forbid arm switching instead, seal it: `kind string? #sealed`)
shadow.model.nml:6:1: note: oneof 'record' selects its arm by 'kind' here
for more information, run: nml explain NML2054
error: 1 error(s)
$ nml fix --dry-run --root . shadow.model.nml
--- a/shadow.model.nml
+++ b/shadow.model.nml
@@ -1,6 +1,5 @@
 model logEntry:
     // the entry's kind
-    kind string?
     msg string?
 
 oneof record by kind:
would fix shadow.model.nml (1 edit(s))
1 edit(s) would apply across 1 of 1 file(s); 0 diagnostic(s) not auto-fixable
```

A REQUIRED plain field of that name (`kind string`) is refused with its
own consequence — every instance fails a missing-field error on a
property it states — and the same deletion as its fix:

```text transcript=tests/fixtures/shadowed-discriminator/own-required
$ nml check --root . shadow.model.nml
shadow.model.nml:3:5: error[NML2054]: oneof 'record' arm "log": model 'logEntry' declares a required field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so no instance can satisfy it (a missing-field error on a property it states); delete it (to forbid arm switching instead, seal it: `kind string? #sealed`)
shadow.model.nml:6:1: note: oneof 'record' selects its arm by 'kind' here
for more information, run: nml explain NML2054
error: 1 error(s)
$ nml fix --dry-run --root . shadow.model.nml
--- a/shadow.model.nml
+++ b/shadow.model.nml
@@ -1,6 +1,5 @@
 model logEntry:
     // the entry's kind
-    kind string
     msg string?
 
 oneof record by kind:
would fix shadow.model.nml (1 edit(s))
1 edit(s) would apply across 1 of 1 file(s); 0 diagnostic(s) not auto-fixable
```

A REQUIRED seal (`kind string #sealed`) is the same shape wearing the
seal's directive — no instance can satisfy it — and its fix is the `?`
that makes it the sanctioned spelling:

```text transcript=tests/fixtures/shadowed-discriminator/seal
$ nml check --root . sealed.model.nml
sealed.model.nml:2:5: error[NML2054]: oneof 'record' arm "log": model 'logEntry' seals the discriminator with a required field 'kind' — an instance's 'kind' property is always read as the discriminator, so a required field can never be satisfied; declare it optional: `kind string? #sealed` (fix: `?`)
sealed.model.nml:5:1: note: oneof 'record' selects its arm by 'kind' here
for more information, run: nml explain NML2054
error: 1 error(s)
$ nml fix --dry-run --root . sealed.model.nml
--- a/sealed.model.nml
+++ b/sealed.model.nml
@@ -1,5 +1,5 @@
 model logEntry:
-    kind string #sealed
+    kind string? #sealed
     msg string?
 
 oneof record by kind:
would fix sealed.model.nml (1 edit(s))
1 edit(s) would apply across 1 of 1 file(s); 0 diagnostic(s) not auto-fixable
```

When the field reaches the arm through `is`, the row sits at the arm's
`is` reference, names the declaring definition, and notes the field and
the union — with no fix: the field may be live in the mixin's other
users (here `note`), so the remedy is yours to choose:

```text transcript=tests/fixtures/shadowed-discriminator/mixin
$ nml check --root . mixed.model.nml
mixed.model.nml:4:19: error[NML2054]: oneof 'record' arm "log": model 'logEntry' inherits a field 'kind' named like the discriminator from trait 'tagged' — an instance's 'kind' property is always read as the discriminator, so the field can never be set in this arm; rename the discriminator, drop `is tagged`, or delete the field where it is declared
mixed.model.nml:2:5: note: field 'kind' declared here
mixed.model.nml:10:1: note: oneof 'record' selects its arm by 'kind' here
for more information, run: nml explain NML2054
error: 1 error(s)
$ nml fix --dry-run --root . mixed.model.nml
0 edit(s) would apply across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable
```

An instance file learns of the shape through its binding: a package
whose declared source carries it cannot build its validator, so the
file's row is [NML2091](#nml2091) with this finding as its `note:`. No
instance-side row can name the cause — the parse of an instance is
schema-free — so the schema's row is the whole story:

```text transcript=tests/fixtures/shadowed-discriminator/bound
$ nml check --root . tenants/cu/plain.flow.nml
tenants/cu/plain.flow.nml: error[NML2091]: binding 'tenantFlows' of demo.package.nml cannot build its validator: declared source `core` failed to load at core.model.nml:3:5: oneof 'record' arm "log": model 'logEntry' declares a field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so the field can never be set; delete it (to forbid arm switching instead, seal it: `kind string? #sealed`) — the file validates under no binding until the source loads
core.model.nml:3:5: note: oneof 'record' arm "log": model 'logEntry' declares a field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so the field can never be set; delete it (to forbid arm switching instead, seal it: `kind string? #sealed`)
for more information, run: nml explain NML2091
error: 1 error(s)
```

**Fix:** delete the field (`nml fix`), or rename the discriminator; to
forbid arm switching, spell the seal optional: `kind string? #sealed`.


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

**The resolved lane** (RFC 0047): at a boundary that owns a file's
resolution (server boot; the deploy CLI's own files), facets are also
enforced on what `$ENV.KEY` resolves to. That shape of the message
names the **variable and the bound, never the resolved content** —
`'pollInterval' from $ENV.P resolved to a value below the schema's
min = 60s` — because env-resolved text is secret-provenance; check
the variable in your environment (`echo $P`) to see the value.
Boundaries that do not own resolution (the control plane; the editor)
defer `$ENV` values instead, and deserialization reports them.

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

## NML2059

**Unresolved layer reference.** A `uses` ref names no in-scope instance
(RFC 0019). Layer refs are bare identifiers resolved through the file's
scope — same-file declarations, plus import bindings once RFC 0020
lands. The message offers a did-you-mean over in-scope instances of the
same model keyword.

```nml check expect-error='[NML2059]'
model flow:
    entrypoint string

flow memberLookup:
    entrypoint = "search"

flow tenant uses memberLokup:
    entrypoint = "search"
```

**Fix:** spell the ref as the base instance is declared (the hint names
the closest candidate).

## NML2060

**Sealed field violation.** A `#sealed` field is write-once from the
bottom of the layer stack (RFC 0019): the first layer to assign it fixes
it, and any higher assignment is rejected.

Rejection holds **even at the identical
value**, because a restatement silently decouples the moment the base
changes (that form carries a machine-applicable *delete this
assignment* suggestion). The seal binds every field shape — scalars,
lists, and object-typed fields alike (an object body is a write; only a
zero-item entry on a *list*-shaped sealed field is not, per NML2079's
contract). The third form is the **seal backstop**, and it binds all
three variant forms equally: a oneof arm change, a union `as` switch
(RFC 0015 — the lowest supplying layer establishes the variant, an
un-annotated upper body never switches, and the resolved body carries
the variant as an explicit annotation), and arm-set wholesale
replacement (RFC 0007) — any of them discarding a lower body containing
an assigned sealed field, at any depth (nested objects and nested oneof
arms included — and interiors reached through union-typed fields,
union-typed LIST elements and a union's own list variant (item paths
render as `slot[w].field`), arm-set inline arm bodies, and
oneof-targeted arm sets), is this error;
the message names the first discarded seal and counts the DISTINCT
sealed fields that follow (`'slot[w].secret' (and 3 more fields)`),
adding the assignment count when it exceeds the fields (two layers
assigning one sealed field are `(2 assignments)`); one `sealed here`
note points at each of the first four assignments, each locating its
own file, and it names the switch it refused (the discriminator value,
the `as` target, or the replacement), and closes with the action:
compose into the lower value, or unseal the field in the schema. A type-annotation modifier (`|slot (a | b)`) inside an
instance body is a declaration, never a write: it neither seals nor
violates a seal.

```nml check expect-error='[NML2060]'
model flow:
    entrypoint string #sealed

flow base:
    entrypoint = "search"

flow hijacked uses base:
    entrypoint = "adminPanel"
```

**Fix:** delete the overlay assignment — a sealed value is the lower
layer's to change. If the field must vary per tier, the schema author
removes `#sealed` or leaves the field unassigned at the base (a sealed
field no layer assigns stays open for the next tier).

## NML2061

**Layer reference cycle.** A `uses` stack loops back on itself
(RFC 0019). Composition is a DAG; the cycle renders as its full path in
a canonical rotation (smallest member name first), so the same cycle
discovered from every entry point is ONE finding, not one per member.

```nml check expect-error='[NML2061]'
model thing:
    v string

thing a uses b:
    v = "a"

thing b uses a:
    v = "b"
```

**Fix:** break the cycle — one of the two instances is the base and
must not `uses` the other.

## NML2062

**Layer keyword mismatch.** Every layer in a stack must declare the
same model keyword as the composing block (RFC 0019) — layers compose
only within one model. The code also covers a `uses` clause on a schema
definition (`model` / `trait` / `enum`), which cannot compose layers.

```nml check expect-error='[NML2062]'
model thing:
    v string

model other:
    w string

other base:
    w = "b"

thing t uses base:
    v = "t"
```

**Fix:** reference an instance of the same keyword, or move the shared
content into one. On a schema definition, delete the `uses` clause —
the suggested fix is structural (`nml fix` and the editor remove the
clause, its separators, and the colon rule on a bodyless header).

## NML2063

**Illegal identity redefinition.** Three shapes, one rule — list items
merge by identity, and an identity may only be redefined where the
schema grants it (RFC 0019): an item redefining an existing identity
under `#append` **without** `#identity` (the schema grants adding, not
overriding); a **cross-kind** match at an equal token (a named
`- search:` against a scalar `- "search"`, or any item at a bodiless
reference/role item's token — match the base's spelling); or a
**duplicate identity within one layer's own list** (the merge key must
be unique before it can be merged on).

```nml check expect-error='[NML2063]'
model step:
    name string+
    action string

model flow:
    steps []step #append

flow base:
    steps:
        - audit:
            action = "log"

flow t uses base:
    steps:
        - audit:
            action = "noop"
```

**Fix:** for the `#append` shape, ask the schema owner for `#identity`
if overriding is legitimate; for a cross-kind match, restate the item in
the base's spelling; for a within-layer duplicate, delete it.

## NML2064

**Composition not permitted.** The file's governing context denies
composition outright (RFC 0019). Two shapes, each naming its owner: the
governing binding carries no `layers:` grant (names the binding and its
manifest — for the operator's own workspace manifest "an operator
change, not fixable from a content file", with the remedy located and
structured below; for a manifest from outside the walk the sentence
says where it lives and what change it needs: the store's current copy
of a package — add the grant in the package's source and republish it;
a package embedded in the binary by its embedder — add it there and
rebuild; nml's own builtin package, which grants no composition to the
manifests it binds — write the manifest without `uses`); or no
binding governs the file in a closed universe (names how many manifests
were discovered; the universe's root is the run's own fact — `nml
binding`'s `root` line and the closing `summary` row spell it). Both
end by pointing at `nml binding <file>`. A file two live manifests
claim is a different denial — NML2087, reported before the file is
read, so a composing file never reaches this code. In the open
developer context (no manifest anywhere within the root's fence)
composition is permitted and this code never fires. The universe is
fixed once per invocation — `--root`, else the outermost manifest or
`nml-project.nml` between the checked file and its `.git` fence — so a
content file can never re-root it.

A manifest whose binding claims the file but grants nothing:

```nml fragment
[]validator validators:
    - tenantFlows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - core
        // no `layers:` grant → any `uses` clause here is NML2064
```

```text transcript=tests/fixtures/workspace
$ nml check --root . tenants/cu/member-lookup.flow.nml
tenants/cu/nml-project.nml: warning[NML2080]: project config `tenants/cu/nml-project.nml` is inert: it sits inside content claimed by binding 'tenantFlows' of demo.package.nml (files[0] = "tenants/**/*.flow.nml") — content, not configuration; its pins, autoAssociate, bindings and anchoring are ignored
tenants/cu/member-lookup.flow.nml:4:7: error[NML2064]: composition not permitted: binding 'tenantFlows' (demo.package.nml) carries no `layers:` grant — an operator change, not fixable from a content file; run `nml binding tenants/cu/member-lookup.flow.nml` to see the effective grant
demo.package.nml:10:7: note: to permit it, give this binding a `layers:` grant whose `allowRefs` admits "tenants/cu/member-lookup.flow.nml"
help: the block to add after line 15 of demo.package.nml — paste it as printed:
        layers:
            allowRefs:
                - "tenants/cu/member-lookup.flow.nml"
for more information, run: nml explain NML2064
error: 1 error(s)
```

The remedy is LOCATED and STRUCTURED: the `note:` sits at the binding
in its manifest (an editor lands there as a related location) and names
the exact key an `allowRefs` entry must admit — the referenced
instance's defining file — and the block itself travels as the
finding's machine-applicable edit in that manifest. A finding's message
is one line by contract, so the block is the `help:` beneath the row,
resolved against the manifest: printed at the binding body's own
indentation and naming the line it goes after — copy its three lines as
printed and paste them there (a block pasted at the list's item column
would be no entry of the binding). On `--json` the row's
`suggestions[]` carries the same edit (the manifest's key, the
insertion point, the lines); the editor offers it as a quick-fix on the
manifest. The same block lives here, for a manifest formatted as
`nml fmt` writes one:

```nml fragment
        layers:
            allowRefs:
                - "tenants/cu/member-lookup.flow.nml"
```

With the block in place the file composes and `nml binding <file>`
prints the grant as loaded — here on a manifest whose `vendorFlows`
binding carries one (`tenantFlows` beside it does not):

```text transcript=tests/fixtures/workspace-grant
$ nml check --root . vendor/base.flow.nml
vendor/base.flow.nml: ok (2 declaration(s))
$ nml binding --root . vendor/base.flow.nml
file      vendor/base.flow.nml
root      .  (--root)
binding   vendorFlows   demo blake3:186aeb08, workspace manifest (demo.package.nml)
anchor    .   matched files[0] = "vendor/**/*.flow.nml"   (auto-associated)
layers    granted
          allowRefs[0] = "vendor/**"
          denyRefs[0] = "vendor/vetoed/**"
          maxStackDepth = 4
```

**Fix:** an operator adds that `layers:` grant to the governing binding
— the `help:` beneath the finding is the block to paste, its `allowRefs`
entry the exact key the note names (the file's own while its refs are
same-file), or a glob over the subtree the binding delegates
(`"tenants/**"`); content authors cannot grant themselves composition,
and a manifest inside claimed content is inert (NML2080). The grant's
own rules are checked at load (NML2081); `nml binding <file>` prints
the grant as loaded.

## NML2065

**Layer reference denied by grant.** A referenced layer falls outside
the governing `layers:` grant (RFC 0019). Two forms: a **deny-veto**
names the vetoing rule by index (`denyRefs[2]` — grant rules are
unnamed, so the index is the stable referent, and `nml binding` prints
the same indices) and may name the path (the allowlist already admits
it); an **allow-miss** states that no `allowRefs` entry admits the
layer, never naming the path (a deny must not become an enumeration
channel). Stack-level denials — a transitively pulled layer the root
grant does not admit — name the entering ref (the root clause's own
listed token, whose span anchors the diagnostic in the checked file);
the stack-level allow-miss form also withholds the denied layer's
*instance name* (author-chosen, so it too must not leak through a
denial the author never spelled). Every form carries the denial
family's contract tail: the binding AND its manifest file named, the
change stated as an operator's, and a closing `nml binding <file>`
pointer with the checked file's real path.

A grant that admits `vendor/**` but vetoes one subtree; a file under
the vetoed subtree composing from itself:

```nml fragment
        layers:
            allowRefs:
                - "vendor/**"
            denyRefs:
                - "vendor/vetoed/**"
```

```text transcript=tests/fixtures/workspace-grant
$ nml check --root . vendor/vetoed/v.flow.nml
vendor/vetoed/v.flow.nml:4:14: error[NML2065]: `uses` ref 'base' denied by denyRefs[0] of binding 'vendorFlows' (demo.package.nml) — an operator change, not fixable here; run `nml binding vendor/vetoed/v.flow.nml`
for more information, run: nml explain NML2065
error: 1 error(s)
```

The same manifest, a file the allowlist admits: it composes clean.

```text transcript=tests/fixtures/workspace-grant
$ nml check --root . vendor/base.flow.nml
vendor/base.flow.nml: ok (2 declaration(s))
```

**Fix:** an operator widens `allowRefs` (or removes the deny rule the
index names) on the governing binding; `nml binding <file>` prints the
rules by the same indices.

## NML2066

**Layer bound exceeded.** A composition bound was hit (RFC 0019), and
the message names **which**: the grant's `maxStackDepth` (an operator
change), the language stack cap (16 distinct instances in one
linearized stack, the declaring instance included — enforced during
discovery *and* before the linearization merge, so an over-wide clause
is rejected in linear time), or the import-closure cap (256 files;
that cap arrives with RFC 0020 imports). The language caps bound merge
work — they are defensive, not product rules.

**Fix:** restructure the stack (fewer tiers), or — for the grant cap —
ask the operator to raise `maxStackDepth`.

## NML2067

**Unmatched overlay item.** In an `#identity` list without `#append`,
an upper-layer item matched no base identity (RFC 0019). Nothing is
implicit: an unmatched item is never a silent addition — a typo'd
identity gets a did-you-mean instead of becoming a stray executable
step. The hint discloses **named** identities only; scalar-keyed tokens
are values and are never echoed.

```nml check expect-error='[NML2067]'
model step:
    name string+
    locator string

model flow:
    steps []step #identity

flow base:
    steps:
        - submitSearch:
            locator = "#submit"

flow t uses base:
    steps:
        - submitSaerch:
            locator = "#x"
```

**Fix:** match an existing base identity (the hint names the closest),
or ask the schema owner for `#append` if adding items is legitimate.

## NML2068

**Invalid merge-policy declaration.** The schema's merge-policy
directives are incoherent (RFC 0019, schema load): `#sealed` composes
with no other policy (write-once contradicts every other grant — seal
the *item fields*, not the list, when that is the intent);
`#identity`/`#append` are list and set policies and are rejected on
scalar and object fields; `#identity` needs items with something to key
and something to merge, so plain scalar lists and `set<T>` are rejected
(`#append` and overlay are the policies that mean something there); and
`#overlay` is the explicit spelling of the default and combines with
nothing. Exactly one policy per field — with one sanctioned pair,
`#identity #append`.

```nml check expect-error='[NML2068]'
model m:
    xs []string #identity

m instanceOfM:
    xs = ["a"]
```

**Fix:** the message names the incoherence; pick the one policy (or the
sanctioned pair) that states the intended grant.

## NML2076

**Unreachable seal** *(warning, schema load)*. A `#sealed` declaration
that cannot engage — a promise of protection the composition rules do
not deliver (RFC 0019).

Three shapes: item-model seals under a
**bare-overlay list** (wholesale replacement drops base items; grant
the list `#identity` to make item seals reachable — for a list whose
element is a union, or a union's own list variant such as `(a | []b)`,
where `#identity` is not grantable, the advice is honest instead: seal
outside the list element; the variant-switch backstop still guards
switches); a **oneof** — as a
field type or an instance root — whose arm models declare sealed fields
while the discriminator is unsealed (an arm switch discards the sealed
body; the seal backstop still rejects a switch that would drop an
*assigned* seal, but sealing the discriminator forbids switching
outright); and `#sealed` on a field **with a schema default** (a
default is not an assignment — the field stays open until some layer
writes it).

**Fix:** the message names the shape and its remedy; the common one is
granting the list `#identity`, sealing the discriminator, or dropping
the default and assigning explicitly at the base. Sealing the
discriminator is spelled on each arm model as an **optional** field
(`kind string? #sealed`) — an instance's property of that name is
always claimed as the discriminator, so a required spelling could never
be satisfied (NML2054 refuses it, the `?` as its fix); the optional
sealed form is the one discriminator-named field an arm may declare.

## NML2077

**No consistent linearization.** The `uses` DAG's declared orders
contradict, and the C3 merge refuses rather than guessing (RFC 0019).

Stack order decides which assignment holds a seal, so it is never
resolved by heuristic. Three shapes: a listed layer is a transitive
base of an earlier-listed layer (the clause asks for it both above and
below its dependent); two listed layers' own stacks order a shared pair
oppositely; or the orders ROTATE across three or more clauses — the
diagnostic names the cycle's pairwise steps and which clause forces
each. The diagnostic names the contradicting pair and its cause;
the transitive-base shape carries a machine-applicable remove-the-ref
fix — structural: `nml fix` and the editor delete the reference with
its separator — and the sibling shape offers both reorderings as
hints. Note redundancy and contradiction
are orthogonal: the same ref
listed in the dependency-consistent position is redundant but legal,
and deliberately silent.

```nml check expect-error='[NML2077]'
model thing:
    v string

thing base:
    v = "b"

thing mid uses base:
    v = "m"

thing top uses mid, base:
    v = "t"
```

**Fix:** reorder or drop the contradicting ref (here: `uses base, mid`
— or just `uses mid`, which already contains `base`).

## NML2079

**Zero-item layer entry** *(warning)*. A composing layer's entry at a
list — or at a union position with a list or set variant — normalizes
to zero items and supplies nothing.

At a list field that is a `.shared:`-only block, an empty array literal
(`xs = []`), or an empty nested block (RFC 0019); at a union position,
`= []`, an empty modifier, or an entry-less un-annotated block (a keyed
or annotated block is a model body and a write). It does not count as
supplying the list, so it neither replaces nor empties anything — and
at a union position it never establishes a variant either (the lowest
layer that *supplies* establishes; RFC 0019 errata E7). Because the
author may have *meant* "clear the base list" (an operation with no
merge spelling), it is always diagnosed, never silently ignored — a
discarded body included: it is diagnosed under its own readings
(RFC 0025).

**Fix:** delete the entry, or supply the items the layer should
contribute. Emptying a base list is not expressible; where entries must
be removable, the schema owner models it as data (an `enabled` flag).

## NML2080

**Inert resolution input.** A project config (`nml-project.nml`), a
package manifest (`*.package.nml`) or a root marker sits inside content
that a live outer manifest's binding claims (RFC 0019, *Resolution inputs
must not be author-writable*). It is content, not configuration: its
pins, `autoAssociate`, bindings and anchoring are ignored, and this
warning names the binding and the `files` glob that claims it. The rule
is what keeps binding selection trustworthy — otherwise a one-line
`nml-project.nml` committed under a tenant's subtree would detach that
subtree from the operator's binding into the permissive default, a
same-named manifest would replace the operator's wholesale, and a
marker file would re-anchor a package's globs. Liveness is decided
outermost-first by directory nesting, never by path order: an input is
inert only when a manifest at a strictly shallower directory claims it,
so a sibling can neither inert nor govern anything. A root marker nested
under a live marker of the same package is inert for the same reason.

A tenant's project config committed under content the operator's
binding claims:

```nml fragment
// tenants/cu/nml-project.nml — inside `tenants/**/*.flow.nml`
project p:
    autoAssociate = false
```

```text transcript=tests/fixtures/workspace
$ nml check --root . tenants/cu/plain.flow.nml
tenants/cu/nml-project.nml: warning[NML2080]: project config `tenants/cu/nml-project.nml` is inert: it sits inside content claimed by binding 'tenantFlows' of demo.package.nml (files[0] = "tenants/**/*.flow.nml") — content, not configuration; its pins, autoAssociate, bindings and anchoring are ignored
tenants/cu/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2080
```

The editor reports the note ONCE, on the inert input's own document,
at its declaration, as information — the input's author is the one who
can act, and their document is where they look; the files beneath the
input carry no NML2080 row. The CLI prints it once per run above the
files it bears on.

**Fix:** none is needed for correctness — the input is ignored. Delete
it if it is stray; if the subtree really is separately configured,
narrow the outer binding's `files` glob so it no longer claims it, then
run `nml binding <file>` to see the effective claim.

## NML2081

**Layer grant rule.** A validator binding's `layers:` grant (RFC 0019)
breaks one of the loader's own rules for it: an `allowRefs` or
`denyRefs` glob the matcher rejects (`**` must be a whole segment; every
segment one a key component can equal — never empty, `.`, `..` or
`\`-bearing; at most 64 `/`-separated segments — the rules `files`
globs follow), a
`maxStackDepth` above the language cap of 16, or
`denyRefs` beside an empty `allowRefs` (a veto with nothing to veto —
an allowlist that admits nothing already denies every ref). Refused at
manifest LOAD and located at the offending item, so the manifest binds
nothing until it is fixed: the universe is closed-denied around it
exactly as under NML2088 — nothing under it validates, every verb
exits 1, `nml binding` prints the row. Load-time because an over-cap
glob matches nothing at match time, which is fail-closed for an allow
rule but FAIL-OPEN for a deny rule; the asymmetry is closed before it
can exist. The block's *shape* — an unknown property, a wrong type, a
missing `allowRefs`, `maxStackDepth = 0` or `1.5` (the field is
`number(min = 1, multipleOf = 1)`, RFC 0018 exact) — is the
meta-schema's finding, reported as NML2088 like any other malformed
manifest field.

A grant whose allow glob embeds `**` in a segment:

```nml fragment
        layers:
            allowRefs:
                - "vendor/**x"
```

```text transcript=tests/fixtures/workspace-grant-bad
$ nml check --root . vendor/base.flow.nml
demo.package.nml:23:19: error[NML2081]: manifest failed to load: validator 'vendorFlows' layers.allowRefs[0] = "vendor/**x": `**` must be a whole segment
for more information, run: nml explain NML2081
error: 1 error(s)
```

The other two forms read `layers.maxStackDepth = 17 exceeds the
language cap 16` and `layers.denyRefs has nothing to veto: allowRefs is
empty`, each located at its item — `key:line:col:` on the row, `line`
and `col` on the `--json` wire, the editor's squiggle in the manifest;
the sentence names no line.

**Fix:** spell the glob as the matcher reads it (`**` alone in its
segment, `/`-separated plain segments — never `\`, `.` or `..` — at
most 64 segments), keep `maxStackDepth` at most 16 (whole and at least
1 is the meta-schema's rule), and list at least one `allowRefs` entry
beside any `denyRefs`
(or drop the veto); `nml binding <file>` then prints the grant as
loaded.

## NML2082

**Reserved directive name.** *(Manifest load.)* A package's `[]directive`
vocabulary declares `sealed`, `identity`, `append` or `overlay` — the
language's four merge-policy directives (RFC 0019). They are part of every
vocabulary already and carry the language's meaning, so a package cannot
redeclare one under its own. Refused at manifest load, at the entry, so
the manifest binds nothing until it is fixed: the row is reported under
this code — the rule's own, as the grant's NML2081 is — on the command
line, on the `--json` wire and in the editor, and every file the manifest
governs is denied with it.

A manifest declaring `sealed` under its own meaning:

```nml fragment
[]directive directives:
    - sealed:
        arg = "none"
        doc = "Write-once, under this package's own meaning."
```

```text transcript=tests/fixtures/directive-collide
$ nml check --root . apps/app.nml
demo.package.nml:17:7: error[NML2082]: manifest failed to load: `[]directive` entry 'sealed' redeclares the language's merge-policy directive `#sealed` — `sealed`, `identity`, `append` and `overlay` are reserved (RFC 0019) and known under every vocabulary; rename it
for more information, run: nml explain NML2082
error: 1 error(s)
$ nml check --root . --json apps/app.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"code":"NML2082","col":7,"line":17,"message":"manifest failed to load: `[]directive` entry 'sealed' redeclares the language's merge-policy directive `#sealed` — `sealed`, `identity`, `append` and `overlay` are reserved (RFC 0019) and known under every vocabulary; rename it","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** rename the directive; the builtin is available to every schema the
package covers without a declaration.

## NML2083

**Content reached through a symlinked path rejected.** Under a closed
universe (one that holds a workspace manifest), the checked file's path
is walked component by component from the workspace root, `lstat` first,
and a symlinked component — an ancestor or the file itself, dangling or
not — halts the walk before its target is ever resolved (RFC 0019
pipeline step P4). A link could relocate content into a differently-
trusted subtree; refusing it makes the attempt loud instead of quietly
re-scoped, and because the target is never touched the message is
byte-identical whether or not it exists — a planted link is not an
existence oracle. Two forms: **form 1** names the symlinked component;
**form 2** ("cannot verify the on-disk spelling of this path on this
backend") fires on a backend without realpath when the path was looked
up under a spelling the filesystem does not list byte-exactly. Open
contexts (no manifest) follow links within the root and never emit this
code.

A tenant commits `tenants/cu/lib` as a symlink to `../../vendor`; the
message is the same whether or not `vendor/base.flow.nml` exists:

```text transcript=tests/fixtures/workspace-link-a
$ nml check --root . tenants/cu/lib/base.flow.nml
tenants/cu/lib/base.flow.nml: error[NML2083]: closed binding rejects `tenants/cu/lib/base.flow.nml`: path component `lib` is a symlink — content reached through a symlinked path is rejected in a closed universe (a link could relocate content into a differently-trusted subtree); replace the link with the content itself, or check the file at its real path
for more information, run: nml explain NML2083
error: 1 error(s)
```

A path spelled through `..` names the spelling as typed beside the key
(`closed binding rejects \`tenants/cu/lib/../plain.flow.nml\` (key
\`tenants/cu/plain.flow.nml\`)`), since the component it names need not
appear in the key.

**Fix:** replace the link with the content itself, or check the file at
its real path; for form 2, spell the path exactly as the filesystem
does.

## NML2084

**Dead delta** *(warning)*. An overlay assignment restates the
effective lower value unchanged (`semantic_eq`) — the
copy-the-whole-body anti-pattern layer composition exists to eliminate
(RFC 0019). The restatement is not wrong today, but it silently
decouples the moment the base changes. Scalar and object fields under
overlay policy; on a `#sealed` field the equal-value NML2060 form takes
precedence — one span, one diagnostic. Never fires on
`#append`/`#identity` lists, whose per-item semantics differ (a
duplicate scalar append is legal).

**Fix:** delete the restatement — an overlay states deltas, and an
absent field inherits the base value automatically.

## NML2085

**Discarded union contribution.** A union-typed position received a
contribution that can neither merge into the established variant nor
switch it (RFC 0015 + RFC 0019).

The lowest supplying layer establishes
the variant — named (its `as` annotation, else its unambiguous body
shape), or structural *per shape* (a scalar value or a list value are
distinct establishments) — and only an authored `as` on a nested body
ever switches. That leaves three irreconcilable pairings, all reported
by this error rather than silently dropped: a whole-value spelling over
an established *named* variant (`as` has no scalar spelling, so it
cannot switch); an un-annotated body over an established *structural*
value (an un-annotated body never switches, and a body cannot merge
into a scalar); and a scalar↔list cross inside the structural bucket (a
list cannot merge into a scalar or vice versa, and structural variants
have no `as` spelling to switch between — scalar variants of different
scalar types, e.g. `(string | number)`, overlay like any scalar). The
message leads with the position and its establishment (by a lower
layer, or by an earlier entry in the same layer), and a related note
marks the establishing entry — "established here" for a body
establishment, "in force here" for the structural value currently in
force (the latest scalar, or the highest list supplier).

At a union position that admits items (one with a list or set variant —
a set variant is reachable by array literal, never by block shape),
zero-item entries (`= []`, an empty block, a `.shared`-only block)
never supply and never establish — they are NML2079's warned no-ops
here too, and a position only they supply survives as `= []`; a keyed
or annotated block is a model body and a write, seal included. An
un-annotated body is *inferred* only where the union has one nameable
variant — with two or more, the D2 oracle calls it ambiguous.
Composition never *guesses* a variant: a keyed body the D2 oracle calls
ambiguous composes model-less and un-annotated, so NML2052 fires on the
composed view exactly as it would on the raw one; an authored `as`
above an ambiguous group *pins* it (it resolves the ambiguity rather
than switching away from a variant never chosen, and its own identifier
becomes the composed annotation) — except at a `#sealed` union
position, where the pin is a second assignment and the seal rejects it
(NML2060).

A switch away from a list-value establishment is judged over the list
the displaced compose would carry — the highest layer's list, under the
union's first `List` variant (the one block-shaped items resolve to; a
set variant is never selected by block shape, so it is never the
judging vocabulary).

```nml check expect-error='[NML2085]'
model card:
    last4 string

model account:
    payment (card | string)

account base:
    payment as card:
        last4 = "4242"

account t uses base:
    payment = "cash"
```

**Fix:** compose into the established variant (match its shape), or
switch deliberately — an authored `as <Variant>` on a nested body — or
restate the structural value where one is established.

## NML2086

**Internal composition invariant violated.** The compose engine reached
a decision it holds to be unreachable.

Example: a union-only verdict at a oneof position. The engine fails safe
and loud rather than composing something silently wrong — the message
names the position and says what happened to the layer's contribution:
it was not composed. It should never
appear on valid or invalid input alike. The editor guards its compose
pass the same way: an internal error degrades to raw-text findings plus
this code at the top of the buffer, never a dark buffer.

**Fix:** none on your side — please report the input that produced it.

## NML2087

**Ambiguously claimed file.** Two or more live workspace manifests
claim the checked file through their `files` globs (RFC 0019 item 0,
rule 3). The file is *denied*: it validates under no binding and
nothing runs against it — never a nearest-wins shadow, never a
parse-only pass with a green exit. The finding is reported before the
file is read, names every claimant as `<manifest> (<binding>, files[i]
= "<glob>")` in manifest order, and every verb exits 1 on it: `check`,
`validate` and `binding` on the denial, `fix` counting the file as a path
that could not be fixed (its `--check` gate likewise). A `schemaPackages`
pin in the nearest live project config
chooses between package *names*, never between two manifests of one
name, so a pin cannot resolve it.

Two manifests whose globs overlap on `shared/`:

```nml fragment
// demo.package.nml            // other.package.nml
[]validator validators:        []validator validators:
    - shared:                      - sharedToo:
        files:                         files:
            - "shared/**/*.flow.nml"       - "shared/**/*.flow.nml"
```

```text transcript=tests/fixtures/workspace
$ nml check --root . shared/x.flow.nml
shared/x.flow.nml: error[NML2087]: 2 manifests claim this file: demo.package.nml (shared, files[0] = "shared/**/*.flow.nml"), other.package.nml (sharedToo, files[0] = "shared/**/*.flow.nml") — an ambiguously-claimed file is denied: it validates under no binding and nothing runs against it; remove or narrow one claim (a `schemaPackages` pin in the nearest live project config chooses between package names, never between two manifests of one name)
for more information, run: nml explain NML2087
error: 1 error(s)
$ nml binding --root . shared/x.flow.nml
file      shared/x.flow.nml
root      .  (--root)
binding   AMBIGUOUS — 2 manifests claim this file: demo.package.nml (shared, files[0] = "shared/**/*.flow.nml"), other.package.nml (sharedToo, files[0] = "shared/**/*.flow.nml")
layers    none — the file is denied (NML2087)
notes     shared/x.flow.nml: error[NML2087]: 2 manifests claim this file: demo.package.nml (shared, files[0] = "shared/**/*.flow.nml"), other.package.nml (sharedToo, files[0] = "shared/**/*.flow.nml") — an ambiguously-claimed file is denied: it validates under no binding and nothing runs against it; remove or narrow one claim (a `schemaPackages` pin in the nearest live project config chooses between package names, never between two manifests of one name)
for more information, run: nml explain NML2087
```

**Fix:** an operator removes or narrows one of the claiming globs so
exactly one binding claims the file; `nml binding <file>` lists every
claimant with the glob index that matched.

## NML2088

**Resolution input failed to load.** A *live* package manifest or
project config under the workspace root could not be loaded (RFC 0019
item 0, E27(3)): unreadable, not UTF-8, malformed, failing manifest
validation, past its byte cap (256 KiB for a manifest or a project
config; a declared schema source is read up to 4 MiB), declaring a name
that does not match its file stem, or declaring a schema source that is
absent, symlinked or oversized. An input that cannot be loaded cannot
be trusted to say what it governs, so the universe is *closed-denied*:
no binding governs any file under it and every verb exits 1 — never a
silent fall-through to the permissive open context. An *inert* input
(one a shallower live manifest's binding claims, NML2080) is never
loaded and cannot raise this code — a tenant's malformed manifest
inside claimed content does not fail the operator's checks. A manifest
that fails to parse or to meta-validate is reported AT its first
finding — `tenant.package.nml:3:1: error[NML2088]: manifest failed to
load: indentation of 2 matches no enclosing block (open blocks are at
columns 0, 4)` — the manifest's key, line and
column as the row's own location (`line`/`col` on the `--json` wire,
the editor's squiggle in the manifest), the place to open next
(`(finding 1 of N)` follows the failure when the manifest has more than
one). On the `--json` wire that
finding rides the row as `cause` — `{code, source, line, col,
message}`, its code a fact beside the row's own (NML2093 for a
repeated entry, NML1000 for a repeated declaration, NML2001 for an
unknown property, a parse code) — every finding has one: the parser's,
the meta-schema's, or the loader's own (NML2094–NML2104: a keyword
declared twice, a name no schema declares, the `formatVersion` gate).
The finding's remedy rides the row too: a did-you-mean in the manifest
is the row's `suggestions[]` entry, in the manifest's file (`source`),
so the line carries the hint, `nml fix` reports the edit as pending
there (`routed` on its closing row — a run under a manifest that
failed to load rewrites nothing), and the editor offers the quick fix
on the manifest from the manifest document and from every file it
would govern:

```nml fragment
package demo:
    versio = "0.1.0"               // `version`
    formatVersion = 1
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root did-you-mean did-you-mean/tenants/cu/plain.flow.nml
demo.package.nml:2:5: error[NML2088]: manifest failed to load (finding 1 of 2): unknown property 'versio' (not defined in model 'package') (did you mean "version"?)
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root did-you-mean did-you-mean/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2001","col":5,"line":2,"message":"unknown property 'versio' (not defined in model 'package')","source":"demo.package.nml"},"code":"NML2088","col":5,"line":2,"message":"manifest failed to load (finding 1 of 2): unknown property 'versio' (not defined in model 'package') (did you mean \"version\"?)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[{"edits":[{"col":5,"endCol":11,"endLine":2,"line":2,"lines":["version"]}],"kind":"didYouMean","source":"demo.package.nml"}],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/did-you-mean","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

```text transcript=tests/fixtures/manifest-rules
$ nml fix --json --root did-you-mean did-you-mean/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2001","col":5,"line":2,"message":"unknown property 'versio' (not defined in model 'package')","source":"demo.package.nml"},"code":"NML2088","col":5,"line":2,"message":"manifest failed to load (finding 1 of 2): unknown property 'versio' (not defined in model 'package') (did you mean \"version\"?)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[{"edits":[{"col":5,"endCol":11,"endLine":2,"line":2,"lines":["version"]}],"kind":"didYouMean","source":"demo.package.nml"}],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"budgetExhausted":0,"closure":"unloadable","dryRun":false,"edits":0,"errors":1,"exit":1,"failed":0,"files":0,"filesFixed":0,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","remaining":0,"revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/did-you-mean","shadowed":null},"routed":1,"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"suppressed":0,"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"fix","warnings":0,"withheld":null}
```

A manifest whose declared source is missing:

```nml fragment
[]schema schemas:
    - core:
        file = "core.model.nml"    // not beside the manifest
```

```text transcript=tests/fixtures/workspace-unloadable
$ nml check --root . tenants/cu/plain.flow.nml
demo.package.nml: error[NML2088]: manifest failed to load: declared source `core.model.nml` (schemas[0].file in `demo.package.nml`) is unavailable: absent — create it beside the manifest, or fix the `file` path
for more information, run: nml explain NML2088
error: 1 error(s)
```

The reader's byte caps refuse under the same code, naming the size and
the bound in both spellings — a manifest past 256 KiB (here a 300 KiB
one, made in a temporary copy of the fixture, since git holds no
300 KiB manifest):

```text transcript=tests/fixtures/workspace-units sparse=demo.package.nml:307200
$ nml check --root . tenants/cu/plain.flow.nml
demo.package.nml: error[NML2088]: manifest failed to load: too large: 300 KiB (307200 bytes) — a package manifest is read only up to 256 KiB (262144 bytes)
for more information, run: nml explain NML2088
error: 1 error(s)
```

and a declared schema source past 4 MiB, attributed to the manifest that
declares it:

```text transcript=tests/fixtures/workspace-units sparse=core.model.nml:5242880
$ nml check --root . tenants/cu/plain.flow.nml
demo.package.nml: error[NML2088]: manifest failed to load: declared source `core.model.nml` (schemas[0].file in `demo.package.nml`) is unavailable: too large: 5 MiB (5242880 bytes) — a declared schema source is read only up to 4 MiB (4194304 bytes)
for more information, run: nml explain NML2088
error: 1 error(s)
```

The check TARGET's own cap (16 MiB) is the one refusal in this family
without a code: the target is the invocation's input, not a resolution
input, so it fails as a read of that file — exit 1, nothing else denied:

```text transcript=tests/fixtures/workspace-units sparse=tenants/cu/huge.flow.nml:17825792
$ nml check --root . tenants/cu/huge.flow.nml
error: failed to read tenants/cu/huge.flow.nml: too large: 17 MiB (17825792 bytes) — a check target is read only up to 16 MiB (16777216 bytes)
```

A FIFO, socket or device named as the target is refused the same way,
before it is ever opened (a read of a FIFO would block forever):

```text transcript=tests/fixtures/workspace-units fifo=tenants/cu/pipe.flow.nml
$ nml check --root . tenants/cu/pipe.flow.nml
error: failed to read tenants/cu/pipe.flow.nml: `pipe.flow.nml` is not a regular file — a FIFO, socket or device is never opened; replace it with a regular file
```

**Fix:** the message names the input, the reason and the next step — a
declared source by its `schemas[i].file` locator in the manifest (create
it, replace a link with the file, name a regular file, or fix the
`file` path); a manifest or config refused by the reader by its size and
the bound (`too large: 300 KiB (307200 bytes) — a package manifest is
read only up to 256 KiB (262144 bytes)`), its stem, or its first
finding at `<key>:<line>:<col>`. If it is stray content, narrow the
outer binding's glob so it becomes inert; `nml binding <file>` shows the
universe's load errors under `notes`.

## NML2089

**Universe not enumerable.** Discovery under the workspace root was cut
short (RFC 0019 item 0, A16 and its E38 amendment). Two shapes, one
code. *A budget unit is spent:* a tenant-shaped subtree (each directory
a binding glob reaches at the start of its last run of wildcard
directory segments — `tenants/<x>` under `tenants/**/*.flow.nml`) held
more than 65,536 entries, read more than 64 MiB of live manifests,
project configs and declared sources, or holds a directory that could
not be read. Every file under that unit is denied — this error, on the
file, naming the unit, the bound and where the walk stopped — and
nothing else is: the sibling tenant and the operator's own files
validate in the same run. *The universe is truncated:* the root's own
content reached one of the same bounds, or more than 1,048,576 entries
or 1 GiB of live inputs in all were visited (each summed over every
unit). A universe the walk did not finish might hold a
manifest it never saw, so it is treated as *closed-denied* in full — the
row on the root reads ``cannot enumerate manifests: the walk stopped at
…``, no binding governs any file and every verb exits 1.
Directories that are never resolution inputs (`node_modules`,
`target`, dot-directories, `.git`) are skipped by policy and never
count — and are reported, with every symlink, `.nml` FIFO and `.nml`
dot-file the walk left out, on the closing row's `skipped` list; under a
directory target the walking verbs fail on the `.nml` content among them
(NML2090).

A tenant commits a directory of 65,537 files under its unit (the glob is
`tenants/**/*.flow.nml`, so the unit is `tenants/cu`; the docs test
floods a temporary copy of the fixture — git holds no 65,537 files —
and runs this):

```text transcript=tests/fixtures/workspace-units flood=tenants/cu/spam:65537
$ nml check --root . tenants/cu/plain.flow.nml
tenants/cu/plain.flow.nml: error[NML2089]: the discovery budget for `tenants/cu` is exhausted: the walk stopped at `tenants/cu/spam` (the 65536-entry bound for this subtree was reached) — every file under `tenants/cu` is denied and validates under no binding; files outside `tenants/cu` are unaffected; reduce the number of entries under `tenants/cu`
for more information, run: nml explain NML2089
error: 1 error(s)
$ nml check --root . tenants/du/plain.flow.nml
tenants/du/plain.flow.nml: ok (1 declaration(s))
```

The same flood OUTSIDE every unit — beside the manifest, or at
`tenants/cu/spam` under a glob whose units are `tenants/<x>/flows/<y>`
(`tenants/*/flows/**/*.flow.nml`) — is the root's own content and
truncates the whole universe: the row is attributed to the directory the
walk stopped in, names the root (`${ROOT}` stands for its absolute
path), and every file under the root is denied.

```text transcript=tests/fixtures/workspace-units flood=spam:65537
$ nml check --root . tenants/du/plain.flow.nml
spam: error[NML2089]: cannot enumerate manifests: the walk stopped at `spam` (the 65536-entry bound was reached) — the universe is treated as closed and no binding governs any file; remove what stopped the walk, or pass --root to a smaller tree that still holds your manifests
for more information, run: nml explain NML2089
error: 1 error(s)
```

**Fix:** for a spent unit, remove the entry flood or the oversized live
inputs from that subtree, or make its unreadable directory readable
(`--root` is not the remedy — rooting inside the unit leaves the
operator's manifest outside the universe); for a truncated universe,
pass `--root` to a smaller tree that holds the manifests you mean, make
the unreadable directory readable, or shrink the live inputs under the
root.

## NML2090

**Content the walk skipped, unjudged.** A directory named on the command
line expands to the `.nml` files the universe walk enumerated under it
— never a symlink (whatever it names), a FIFO, a dot-file, or anything
under a dot-directory or a policy-skipped `node_modules`/`target`. The
walk REPORTS what it left out on the closing row's `skipped` list, and
under a directory target the walking verbs (`check`, `validate`, `fix
--check`) fail on the `.nml` content among them: a `.nml` FIFO, socket
or device; a `.nml` dot-file; a dot-directory holding `.nml` files,
links or FIFOs — ONE row per directory, naming it, the exact count of
`.nml` files beneath it (`at least` that many when a directory beneath
could not be listed — that directory is its own row) and up to eight of
their keys (a bounded,
breadth-first audit of the hidden subtree — `.git` and policy
directories excepted; 300,000 committed hidden files are one row, not
300,000); a hidden directory the audit could not finish; and a
symlinked `.nml` (in a CLOSED universe the resolver's own
NML2083, the sentence the link gets when named; in an open one this
code, since a link is followed only when named). A symlink whose name
is not `.nml`-shaped is a warning: what lies beneath it is exactly what
the walk never learns. An entry whose NAME no key can carry — not
UTF-8, or bearing a path separator (`ev\il`: a legal name git tracks
on Linux) — is an error under the directory holding it, naming the
entry and what it is (a directory the walk never entered, a symlink, a
`.nml`-named file or special entry; a `.txt` so named is no content
and no row); inside a hidden directory such a `.nml` name is counted
and such a directory leaves the audit incomplete. A directory at the
64-component bound is never listed (nothing beneath it is keyable) and
is an error at its own key; inside a hidden directory it leaves the
audit incomplete. A FILE target gates nothing: the file itself is
judged.

A tenant commits `tenants/cu/link.flow.nml` as a symlink and a
`.hidden/x.flow.nml`; the docs test plants both in a temporary copy:

```text transcript=tests/fixtures/workspace-units link=tenants/cu/link.flow.nml:plain.flow.nml sparse=tenants/cu/.hidden/x.flow.nml:0
$ nml fix --check --root . tenants
tenants/cu/.hidden: error[NML2090]: the walk skipped `tenants/cu/.hidden`: a dot-directory it never enters, holding 1 `.nml` file(s) no verb judged (`tenants/cu/.hidden/x.flow.nml`) — content a runtime could read; move it where the walk lists it, or name the files on the command line
tenants/cu/link.flow.nml: error[NML2083]: closed binding rejects `tenants/cu/link.flow.nml`: path component `link.flow.nml` is a symlink — content reached through a symlinked path is rejected in a closed universe (a link could relocate content into a differently-trusted subtree); replace the link with the content itself, or check the file at its real path
0 edit(s) would apply across 0 of 2 file(s); 0 diagnostic(s) not auto-fixable
for more information, run: nml explain NML2090
error: 2 skipped path(s) hold content no verb judged; nothing was written
$ nml check --root . tenants/cu/plain.flow.nml
tenants/cu/plain.flow.nml: ok (1 declaration(s))
```

In an OPEN universe (no manifest under the root) the walked link is
this code — named on the command line it would be followed:

```text transcript=tests/fixtures/workspace-open link=l.flow.nml:a.flow.nml
$ nml check --root . .
l.flow.nml: error[NML2090]: the walk skipped `l.flow.nml`: a symlink — followed only when named on the command line, never by a directory walk — content a runtime could read that no verb judged; name it, or replace the link with the content itself
./x.nml: ok (3 declaration(s))
for more information, run: nml explain NML2090
error: 1 skipped path(s) hold content no verb judged
```

An entry whose name no key can carry, and a directory at the
64-component bound — the docs test plants a `tenants/ev\il/` directory
holding a `.nml` file, then a tenant directory 62 levels deep, each in
a temporary copy; both are errors at the directory that holds them, and
the closing `--json` row lists them as `unkeyableName` (with `entry`,
the name) and `componentBound`:

```text transcript=tests/fixtures/workspace-units sparse=tenants/ev\il/x.flow.nml:0
$ nml check --root . tenants
tenants: error[NML2090]: the walk skipped an entry under `tenants` whose name no key can carry (`ev\il`: not UTF-8, or bearing a path separator): a directory the walk never entered — content a runtime could read that no verb judged; rename it with a plain name
tenants/cu/plain.flow.nml: ok (1 declaration(s))
tenants/du/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2090
error: 1 skipped path(s) hold content no verb judged
```

```text transcript=tests/fixtures/workspace-units sparse=tenants/cu/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/deep.flow.nml:0
$ nml check --root . tenants
tenants/cu/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d: error[NML2090]: the walk skipped `tenants/cu/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d/d`: a directory at the 64-component bound the walk never enters (nothing beneath it is keyable) — content a runtime could read that no verb judged; flatten the tree, or move its content where the walk lists it
tenants/cu/plain.flow.nml: ok (1 declaration(s))
tenants/du/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2090
error: 1 skipped path(s) hold content no verb judged
```

**Fix:** replace the link with the content itself; replace the FIFO
with a regular file; move hidden content where the walk lists it, or
name the file on the command line (a dot-file named explicitly is the
operator's); for a hidden directory too large to audit, remove it or
move its content out; rename an entry whose name no key can carry;
flatten a tree that nests 64 directories deep.

## NML2091

**Binding cannot build its validator.** The binding that governs the
file is live and unambiguous, but its package does not compose: a
declared schema source fails to load — a parse error, a duplicate
definition, an inheritance cycle (RFC 0019 item 0). The manifest itself
loaded, so the universe stays closed and the binding's claim stands;
what is missing is the vocabulary, and a file cannot be validated under
a vocabulary that does not exist. So the file validates under *no*
binding — never under a registry, never parse-only — and the row names
the binding, its manifest and the source's first finding where it sits
(`<source key>:<line>:<col>`), carried as a `note:` line (the editor's
`relatedInformation`) so the reader can jump to it, and as the row's
`cause` under `--json` (the source's code, sentence and place as
facts). The manifest's
*other* bindings, whose sources load, are unaffected, and the source
document itself keeps its own findings: it is where the operator
repairs the load. `nml binding <file>` shows the row under `notes`.

Both front ends give this one verdict.

```text transcript=tests/fixtures/workspace-brokensrc
$ nml check --root . tenants/cu/plain.flow.nml
tenants/cu/plain.flow.nml: error[NML2091]: binding 'tenantFlows' of demo.package.nml cannot build its validator: declared source `core` failed to load at core.model.nml:3:1 (finding 1 of 5): indentation of 2 matches no enclosing block (open blocks are at columns 0, 4) — the file validates under no binding until the source loads
core.model.nml:3:1: note: indentation of 2 matches no enclosing block (open blocks are at columns 0, 4)
for more information, run: nml explain NML2091
error: 1 error(s)
$ nml check --root . docs/readme.doc.nml
docs/readme.doc.nml: ok (1 declaration(s))
```

**Fix:** open the source at the named line and repair it (`nml check
<source>` lists every finding); the binding's files validate again on
the next run. If the source is stray content, narrow the manifest's
`[]schema` declaration or the binding's `schemas` list.

## NML2092

**A binding glob delegates shallower than its inferred budget unit.**
Discovery charges its bounds per *budget unit* — the
directory a binding glob reaches at the start of its LAST run of
wildcard directory segments (`tenants/<x>` under `tenants/**`) — and
everything outside every unit to the root unit, where one tenant's
flood (a directory past the entry bound, an unreadable directory, 64
MiB of live inputs) denies the whole universe. A glob whose FIRST
wildcard directory run is not its last — `tenants/*/flows/**`,
`orgs/*/tenants/**` — starts delegating at `tenants/<x>` while the
inferred unit sits at `tenants/<x>/flows/<y>`, so `tenants/<x>/other/`
is the root unit's: one tenant's entries there deny everyone. This
warning names the glob, the inferred unit and the declaration that
settles it either way — `budgetUnits`, the manifest's explicit unit
spelling (RFC 0019 item 4): anchor-relative directory patterns (`*`
within a segment, no `**`) that REPLACE the inference for that
manifest, refused at load when narrower than the inference (a
declaration never leaves content a glob reaches in the root unit). It
fires under inference only, once per run on the CLI (the manifest
named, the glob located), on the manifest document at the glob in the
editor, and only for a manifest that mints units — an operator's, not
a tenant's manifest committed inside claimed content. With a `**`
before the boundary (`tenants/**/flows/**`, `**/tenants/*/**`) no
fixed depth can be declared: respell the layout with `*` first.

Units may NEST, and one kind of nesting multiplies (RFC 0026 B-3). A
root catch-all (`**/*.model.nml`, unit `*`) beside `tenants/**` (unit
`tenants/*`) keeps `tenants/<x>` the unit — the operator NAMED
`tenants`, pinning the outer unit's wildcard to a directory of their
own; that is the designed layout and no warning. But a second glob
whose inferred unit lies inside `tenants/<x>` without pinning anything
— `tenants/*/plugins/*/**`, a unit per plugin directory of EVERY tenant
— lets every directory the outer unit delegates mint units of its own
beneath it, each with a fresh entry budget: enough plugin directories
carry one tenant's spend past the universe-wide backstop (16 units'
worth) and the WHOLE universe is denied, where one unit per tenant
denies that tenant alone. Such a glob is always a gap (its last
wildcard run starts past a literal that follows the outer unit's
wildcard), so this warning takes its NESTED form there: the outer unit
and its glob named, and the one declaration the loader accepts — the
delegated subtree; the inferred boundary is withdrawn, since declared
beside the outer unit it would be refused at load (NML2088: `unit
"tenants/*/*" nests inside unit "tenants/*"`).

```text transcript=tests/fixtures/workspace-nested
$ nml check --root . tenants/cu/plain.flow.nml
demo.package.nml:17:15: warning[NML2092]: binding 'tenantPlugins' files[0] = "tenants/*/plugins/*/**/*.model.nml": the inferred budget unit is `tenants/*/plugins/*` — content under `tenants/*` outside it stays in the root unit, where one tenant's flood denies everyone, and the unit nests inside `tenants/*` (binding 'tenantFlows' files[0] = "tenants/**/*.flow.nml"), where every directory that unit delegates would mint units of its own beneath it; declare budgetUnits = ["tenants/*"] to keep one unit per delegated subtree (the inferred boundary cannot be declared beside it)
tenants/cu/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2092
```

The plain form, a single glob delegating shallower than its unit:

```text transcript=tests/fixtures/workspace-gap
$ nml check --root . tenants/cu/flows/plain.flow.nml
demo.package.nml:12:15: warning[NML2092]: binding 'tenantFlows' files[0] = "tenants/*/flows/**/*.flow.nml": the inferred budget unit is `tenants/*/flows/*` — content under `tenants/*` outside it stays in the root unit, where one tenant's flood denies everyone; declare budgetUnits = ["tenants/*"] to isolate each delegated subtree, or ["tenants/*/flows/*"] to keep the inferred boundary
tenants/cu/flows/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2092
```

`nml binding` states the universe's word the same way — once, before
its first block and never inside one (a block's `notes` carry what
bears on that file alone):

```text transcript=tests/fixtures/workspace-gap
$ nml binding --root . tenants/cu/flows/plain.flow.nml
demo.package.nml:12:15: warning[NML2092]: binding 'tenantFlows' files[0] = "tenants/*/flows/**/*.flow.nml": the inferred budget unit is `tenants/*/flows/*` — content under `tenants/*` outside it stays in the root unit, where one tenant's flood denies everyone; declare budgetUnits = ["tenants/*"] to isolate each delegated subtree, or ["tenants/*/flows/*"] to keep the inferred boundary
file      tenants/cu/flows/plain.flow.nml
root      .  (--root)
binding   tenantFlows   demo blake3:33482dfd, workspace manifest (demo.package.nml)
anchor    .   matched files[0] = "tenants/*/flows/**/*.flow.nml"   (auto-associated)
layers    none — composition denied (NML2064)
for more information, run: nml explain NML2092
```

The same manifest with the declaration — `budgetUnits = ["tenants/*"]`
under the package block — makes `tenants/<x>` the unit, so a flood
anywhere under one tenant denies that tenant alone:

```text transcript=tests/fixtures/workspace-gap-declared
$ nml check --root . tenants/cu/flows/plain.flow.nml
tenants/cu/flows/plain.flow.nml: ok (1 declaration(s))
```

**Fix:** declare `budgetUnits` in the manifest — the delegated
subtree (`["tenants/*"]`) to isolate each tenant whole, or the
inferred boundary (`["tenants/*/flows/*"]`) to keep it and silence the
warning — or reshape the glob so its last wildcard run starts at the
delegation point (`tenants/**/*.flow.nml`).

## NML2093

**Duplicate entry.** A body declares each name once. A second entry with
the same name in one body — `port` twice, or a block `files:` beside an
inline array `files = […]` (two spellings of one entry) — is an error at
the later entry, with the first as a `note:` (`relatedInformation` in the
editor). Which of the two is meant is unknowable, so no fix is offered.
The rule runs beside every parse — no consumer can skip it — so the
text does not parse: `check` and `validate` stop at it as at any parse
finding, `nml fix` leaves the file alone, `nml fmt` writes nothing, and
an embedder's `nml_core::parse` refuses the text. Names
are exact bytes (`Files` is not `files`); the sigils are namespaces —
`|allow` and `.timeout` are not `allow` and `timeout` — and a modifier's
type declaration beside its value (`|deny []string`, then `|deny = […]`)
is declare-then-assign, not a repeat. A property (`k = v`), a nested
block (`k:`) and a field definition (`k type`) are one name, so a model
that defines a field twice is this error too. List items (`- x`) are
positional and may repeat (a set's uniqueness is NML2030); arms are
keyed by selector (NML2036). The rule needs no schema and judges the
authored body before any composition: an overlay redefining a base
property is composition, not a duplicate; a repeat inside the overlay's
own body is. At the file scope the same rule is NML1000.

```nml check expect-error='[NML2093]'
model service:
    port number

service Api:
    port = 8000
    port = 8001
```

```text transcript=tests/fixtures/dup-names
$ nml parse bare.nml
bare.nml:3:5: error[NML2093]: duplicate entry 'v' — a body declares each name once
bare.nml:2:5: note: 'v' first declared here
for more information, run: nml explain NML2093
error: 1 parse error(s)
```

A manifest names each entry once — the same rule, where the manifest
is parsed — and each `[]schema` source and `[]validator` binding once
(a binding is its name). A manifest with a second `files` entry fails
to load (NML2088, naming the entry and its first spelling), the universe
closed-denied around it:

```nml fragment
[]validator validators:
    - tenantFlows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - core
        strict = true
        files = ["vendor/**/*.flow.nml"]    // the second `files`
```

```text transcript=tests/fixtures/workspace-dup
$ nml check --root . tenants/cu/plain.flow.nml
demo.package.nml:16:9: error[NML2088]: manifest failed to load: duplicate entry 'files' — a body declares each name once (`files:` and `files = …` are two spellings of one entry)
demo.package.nml:11:9: note: 'files' first declared here
for more information, run: nml explain NML2088
error: 1 error(s)
```

**Fix:** delete the entry that is not meant — nothing can choose it for
you.

## NML2094

**Repeated declaration.** A package manifest declares its `package` block and each of its
`[]schema`, `[]validator` and `[]directive` arrays once — one slot per
keyword, whatever the names. A second declaration of one keyword under
another name (`[]validator more:` after `[]validator validators:`) is
refused at the later keyword, the first as a `note:`, and the manifest fails to load (NML2088, this code
its `cause` on the `--json` row). A second declaration under the SAME
name is the file-scope rule, NML1000, refused where the manifest is
parsed.

```nml fragment
[]validator validators:
    - flows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - core

[]validator more:            // the second `[]validator`
    - other:
        files:
            - "x.nml"
        schemas:
            - core
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root repeated repeated/tenants/cu/plain.flow.nml
demo.package.nml:16:3: error[NML2088]: manifest failed to load: `[]validator` is declared twice — a manifest declares `package` and each of `[]schema`, `[]validator` and `[]directive` once
demo.package.nml:9:3: note: `[]validator` first declared here
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root repeated repeated/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2094","col":3,"line":16,"message":"`[]validator` is declared twice — a manifest declares `package` and each of `[]schema`, `[]validator` and `[]directive` once","source":"demo.package.nml"},"code":"NML2088","col":3,"line":16,"message":"manifest failed to load: `[]validator` is declared twice — a manifest declares `package` and each of `[]schema`, `[]validator` and `[]directive` once","related":[{"col":3,"line":9,"message":"`[]validator` first declared here","source":"demo.package.nml"}],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/repeated","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** merge the declarations into one — a second `[]validator` array's
bindings belong in the first.

## NML2095

**Missing declaration.** A manifest needs a `package <name>:` block and at least one `[]schema`
source: the block names the package (its `version`, its
`formatVersion`), the sources are what it delivers. A manifest without
the block, or with no `[]schema` entry (the array absent or empty),
fails to load (NML2088, this code its `cause`) — a package with nothing
to bind is refused, never loaded as empty. Without the block the row
has no place (nothing to point at); without a source it sits at the
package's name.

```text transcript=tests/fixtures/manifest-rules
$ nml check --root no-package no-package/tenants/cu/plain.flow.nml
demo.package.nml: error[NML2088]: manifest failed to load: manifest has no `package <name>:` block
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root no-package no-package/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2095","col":null,"line":null,"message":"manifest has no `package <name>:` block","source":"demo.package.nml"},"code":"NML2088","col":null,"line":null,"message":"manifest failed to load: manifest has no `package <name>:` block","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/no-package","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root no-schema no-schema/tenants/cu/plain.flow.nml
demo.package.nml:1:9: error[NML2088]: manifest failed to load: manifest declares no `[]schema` sources
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root no-schema no-schema/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2095","col":9,"line":1,"message":"manifest declares no `[]schema` sources","source":"demo.package.nml"},"code":"NML2088","col":9,"line":1,"message":"manifest failed to load: manifest declares no `[]schema` sources","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/no-schema","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** add the `package <name>:` block with `version` and `formatVersion`, or
declare the first `[]schema` source.

## NML2096

**Invalid package name.** A package's name is a lowercase identifier — `[a-z][a-z0-9-]*` —
because it becomes a store path component and a written pin entry
(`nml-project.nml`). `package Demo:` or `package my_pkg:` fails to load
(NML2088, this code its `cause`), at the name.

```nml fragment
package Demo:                  // not lowercase
    version = "0.1.0"
    formatVersion = 1
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root package-name package-name/tenants/cu/plain.flow.nml
demo.package.nml:1:9: error[NML2088]: manifest failed to load: package name 'Demo' is not a lowercase identifier ([a-z][a-z0-9-]*)
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root package-name package-name/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2096","col":9,"line":1,"message":"package name 'Demo' is not a lowercase identifier ([a-z][a-z0-9-]*)","source":"demo.package.nml"},"code":"NML2088","col":9,"line":1,"message":"manifest failed to load: package name 'Demo' is not a lowercase identifier ([a-z][a-z0-9-]*)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/package-name","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** rename the block: lowercase letters, digits and `-`, starting with a
letter.

## NML2097

**Unnamed entry.** A `[]schema`, `[]validator` or `[]directive` entry is its name —
`- core:` with a body — never a quoted or positional item: a source is
what a binding's `schemas` names, a binding is what `nml binding`
prints. An entry spelled `- "core":` passes the meta-schema (the
quoted text fills the name) and fails the loader (NML2088, this code
its `cause`), at the item.

```nml fragment
[]schema schemas:
    - "core":                  // an identifier, not a string
        file = "core.model.nml"
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root unnamed unnamed/tenants/cu/plain.flow.nml
demo.package.nml:6:5: error[NML2088]: manifest failed to load: `[]schema` entries must be named items (`- name:`)
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root unnamed unnamed/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2097","col":5,"line":6,"message":"`[]schema` entries must be named items (`- name:`)","source":"demo.package.nml"},"code":"NML2088","col":5,"line":6,"message":"manifest failed to load: `[]schema` entries must be named items (`- name:`)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/unnamed","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** spell the entry as an identifier with a body: `- core:` then its
fields.

## NML2098

**Empty binding.** A validator binding claims files and names schemas — both non-empty:
`files` is what it governs, `schemas` what it validates them against.
A binding with `files = []` or `schemas = []` binds nothing, and a
binding that binds nothing is a mistake rather than a no-op: the
manifest fails to load (NML2088, this code its `cause`), at the
binding's name.

```nml fragment
[]validator validators:
    - flows:
        files:
            - "tenants/**/*.flow.nml"
        schemas = []               // nothing to validate against
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root empty-binding empty-binding/tenants/cu/plain.flow.nml
demo.package.nml:10:7: error[NML2088]: manifest failed to load: `[]validator` entry 'flows' needs non-empty `files` and `schemas`
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root empty-binding empty-binding/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2098","col":7,"line":10,"message":"`[]validator` entry 'flows' needs non-empty `files` and `schemas`","source":"demo.package.nml"},"code":"NML2088","col":7,"line":10,"message":"manifest failed to load: `[]validator` entry 'flows' needs non-empty `files` and `schemas`","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/empty-binding","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** list at least one glob under `files` and at least one declared schema
under `schemas`, or remove the binding.

## NML2099

**Undeclared schema.** A binding's `schemas` entries name the manifest's own `[]schema`
sources by their logical names; a name no `[]schema` entry declares
(`- corr` beside `- core:`) is a dangling reference, and the manifest
fails to load (NML2088, this code its `cause`), at the binding's name.

```nml fragment
[]schema schemas:
    - core:
        file = "core.model.nml"

[]validator validators:
    - flows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - corr                 // no such source
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root undeclared-schema undeclared-schema/tenants/cu/plain.flow.nml
demo.package.nml:10:7: error[NML2088]: manifest failed to load: validator 'flows' names schema 'corr', which no `[]schema` entry declares
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root undeclared-schema undeclared-schema/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2099","col":7,"line":10,"message":"validator 'flows' names schema 'corr', which no `[]schema` entry declares","source":"demo.package.nml"},"code":"NML2088","col":7,"line":10,"message":"manifest failed to load: validator 'flows' names schema 'corr', which no `[]schema` entry declares","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/undeclared-schema","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** name a declared source, or declare it under `[]schema`.

## NML2100

**Invalid binding glob.** A binding's `files` globs follow the matcher's rules: `**` is a whole
segment (`tenants/**`, never `tenants/**x`), a segment is never empty,
`.`, `..` or `\`-bearing, and a pattern has at most 64 segments. A glob
the matcher can never satisfy matches nothing — a binding that silently
claims nothing — so it is refused at load (NML2088, this code its
`cause`), at the glob. The same rule over a grant's `allowRefs` and
`denyRefs` is NML2081.

```nml fragment
        files:
            - "tenants/**x/*.flow.nml"     // `**` is a whole segment
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root binding-glob binding-glob/tenants/cu/plain.flow.nml
demo.package.nml:12:15: error[NML2088]: manifest failed to load: validator 'flows' glob 'tenants/**x/*.flow.nml': `**` must be a whole segment
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root binding-glob binding-glob/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2100","col":15,"line":12,"message":"validator 'flows' glob 'tenants/**x/*.flow.nml': `**` must be a whole segment","source":"demo.package.nml"},"code":"NML2088","col":15,"line":12,"message":"manifest failed to load: validator 'flows' glob 'tenants/**x/*.flow.nml': `**` must be a whole segment","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/binding-glob","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** spell the glob with `/`-separated plain segments and `*`/`**`
wildcards, `**` on its own.

## NML2101

**Budget unit rule.** A declared `budgetUnits` entry is a directory pattern — 1 to 64
`/`-separated segments, each a plain name or `*`, never `**` — naming
the subtrees the walk budgets separately (RFC 0019 item 4); and no
declared unit nests inside another without a literal pinning one of
the outer unit's wildcards (`tenants/*` beside `tenants/*/*`), since
every directory the outer unit delegates would mint units of its own
beneath it, multiplying its share of the walk's budget up to the
universe-wide backstop. A declaration breaking either rule fails the
manifest at load (NML2088, this code its `cause`), at `budgetUnits`.
A gap between the INFERRED unit and the globs is NML2092, a warning.

```nml fragment
package demo:
    version = "0.1.0"
    formatVersion = 1
    budgetUnits = ["tenants/**"]    // one depth, never `**`
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root budget-units budget-units/tenants/cu/plain.flow.nml
demo.package.nml:4:5: error[NML2088]: manifest failed to load: budgetUnits entry "tenants/**": segment "**" — a unit is a directory at ONE depth (plain names and `*`, never empty, `.`, `..`, `**` or `\`-bearing)
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root budget-units budget-units/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2101","col":5,"line":4,"message":"budgetUnits entry \"tenants/**\": segment \"**\" — a unit is a directory at ONE depth (plain names and `*`, never empty, `.`, `..`, `**` or `\\`-bearing)","source":"demo.package.nml"},"code":"NML2088","col":5,"line":4,"message":"manifest failed to load: budgetUnits entry \"tenants/**\": segment \"**\" — a unit is a directory at ONE depth (plain names and `*`, never empty, `.`, `..`, `**` or `\\`-bearing)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/budget-units","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** declare each unit at one depth (`tenants/*`), and pin a nested unit to
a literal directory (`tenants/ops/*`) or declare one of the two.

## NML2102

**Unsupported format version.** A manifest's `formatVersion` is the package format's compatibility
gate: a reader refuses a manifest newer than the format it
understands — checked BEFORE meta-validation, so a newer publisher
degrades an older reader with one precise refusal, never a wall of
unknown-key findings. The manifest fails to load (NML2088, this code
its `cause`; the gate names no line — it is the whole manifest's), and
the editor's package store reports the same for an installed package.
A new manifest KEY is an addition, not a bump (`docs/stability.md`):
this code fires for a change of syntax or of an existing key's meaning.

```nml fragment
package demo:
    version = "0.1.0"
    formatVersion = 99             // this nml supports 1
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root format-version format-version/tenants/cu/plain.flow.nml
demo.package.nml: error[NML2088]: manifest failed to load: package requires formatVersion 99; this nml supports 1
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root format-version format-version/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2102","col":null,"line":null,"message":"package requires formatVersion 99; this nml supports 1","source":"demo.package.nml"},"code":"NML2088","col":null,"line":null,"message":"manifest failed to load: package requires formatVersion 99; this nml supports 1","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/format-version","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** update nml (the reader), or publish for the format version the
reader supports.

## NML2103

**Multiple manifests.** A package directory holds exactly one
`<name>.package.nml`; a directory holding two cannot say which package
it is and fails to load — `manifest failed validation: package directory
holds 2 manifests; exactly one <name>.package.nml is allowed`. The rule
guards the editor's package store (an installed slot, tampered): no CLI
verb loads a package directory, so this section carries no transcript;
the loader's own test (`a_package_directory_with_two_manifests_is_nml2103`)
is its executed example.

**Fix:** remove the manifest that does not belong to the package.

## NML2104

**Template string in a manifest list.** Every list-valued manifest entry —
`files`, `schemas`, `allowRefs`, `denyRefs`, `budgetUnits`, `rootMarkers`,
`modifiers`, `memberKeywords`, `builtinRefs` — holds plain string literals:
each names a file, a key, a unit or a marker. A template string
(`"tenants/{{x}}"`) is a `string` to the meta-schema but names no plain
text — as a `files` glob it claims nothing, as a budget unit it names no
directory, as a `denyRefs` veto it fires on nothing — and is refused at
the element, through the one gate every list read passes (NML2088, this
code its `cause`).

```nml fragment
        layers:
            allowRefs:
                - "**"
            denyRefs:
                - "tenants/cu/{{x}}"     // a template, not a veto
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root template-in-list template-in-list/tenants/cu/plain.flow.nml
demo.package.nml:19:19: error[NML2088]: manifest failed to load: `denyRefs` holds a template string (`{{…}}`) — a manifest list holds plain string literals, and a template names no file, key, unit or marker; spell a literal `{{` as `\u{7B}{`
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root template-in-list template-in-list/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2104","col":19,"line":19,"message":"`denyRefs` holds a template string (`{{…}}`) — a manifest list holds plain string literals, and a template names no file, key, unit or marker; spell a literal `{{` as `\\u{7B}{`","source":"demo.package.nml"},"code":"NML2088","col":19,"line":19,"message":"manifest failed to load: `denyRefs` holds a template string (`{{…}}`) — a manifest list holds plain string literals, and a template names no file, key, unit or marker; spell a literal `{{` as `\\u{7B}{`","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/template-in-list","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** spell the element as a plain literal; a literal `{{` is `\u{7B}{`.

## NML2105

**A declared schema source is not spelled as one.** *(Manifest load.)* A
`[]schema` entry's `file` names a schema source, and a schema source is
spelled `*.model.nml` or `*.schema.nml`. That suffix is the one admission
every reader shares: the walk that finds an undeclared source beside a
manifest, the `--schema` directory scan, and the editor, which gates its
registry, its schema passes, directive completion and hover on it. A
source declared under any other name was a schema to the loader and to
nobody else — `nml check` judged its directives while the editor opened no
pass and gave no row on the same buffer — so the entry is refused at load,
at the `file` value (NML2088, this code its `cause`), and the manifest
binds nothing until it is fixed.

```nml fragment
[]schema schemas:
    - core:
        file = "core.nml"          // not a schema source spelling
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --root schema-source-name schema-source-name/tenants/cu/plain.flow.nml
demo.package.nml:7:16: error[NML2088]: manifest failed to load: `[]schema` entry 'core' declares "core.nml", which is not spelled as a schema source (.model.nml, .schema.nml)
for more information, run: nml explain NML2088
error: 1 error(s)
```

```text transcript=tests/fixtures/manifest-rules
$ nml check --json --root schema-source-name schema-source-name/tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2105","col":16,"line":7,"message":"`[]schema` entry 'core' declares \"core.nml\", which is not spelled as a schema source (.model.nml, .schema.nml)","source":"demo.package.nml"},"code":"NML2088","col":16,"line":7,"message":"manifest failed to load: `[]schema` entry 'core' declares \"core.nml\", which is not spelled as a schema source (.model.nml, .schema.nml)","related":[],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}/schema-source-name","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** rename the file to `<stem>.model.nml` (or `<stem>.schema.nml`) and
declare it under that name; every reader then agrees it is a schema source.

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

**Unknown directive.** *(A schema source a package covers — `nml check`,
`nml validate` and the editor alike.)* `#name` is neither one of the
language's merge-policy directives (`#sealed`, `#identity`, `#append`,
`#overlay` — RFC 0019, known under every vocabulary) nor in the covering
schema package's declared `[]directive` vocabulary. One kernel judge
answers every front end, so the row and its sentence are the same in
`nml check`, `--json` and the editor; a schema source no package covers is
judged under no vocabulary and accepts any directive. Comes with a
sigil-inclusive did-you-mean over every known name (`#lvie` → `#live`,
`#seled` → `#sealed`). A package declaring `live`, a source spelling
`#lvie` beside the language's `#sealed`:

```text transcript=tests/fixtures/directive-vocabulary
$ nml check --root . core.model.nml
core.model.nml:3:22: error[NML5000]: unknown directive '#lvie' (package 'demo') (did you mean "#live"?)
for more information, run: nml explain NML5000
error: 1 error(s)
```

**Fix:** apply the suggestion, or declare the directive in the package's
`[]directive` vocabulary (a language directive needs no declaration —
declaring one is NML2082).

## NML5001

**Directive arity mismatch.** *(A schema source a package covers — every
front end.)* The directive takes no argument but was given one, or
requires one and lacks it — per its declared `arg` kind (the language's
merge-policy directives take none). Judged by the same kernel vocabulary
as NML5000, so `nml check`, `--json` and the editor say one thing.

```nml fragment
[]directive directives:
    - live:
        arg = "none"                 // `#live`, never `#live("…")`
```

```text transcript=tests/fixtures/directive-arity
$ nml check --root . core.model.nml
core.model.nml:3:22: error[NML5001]: '#live' takes no argument
for more information, run: nml explain NML5001
error: 1 error(s)
```

```text transcript=tests/fixtures/directive-arity
$ nml check --json --root . core.model.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"code":"NML5001","col":22,"line":3,"message":"'#live' takes no argument","related":[],"severity":"error","source":"core.model.nml","suggestions":[],"type":"diagnostic"}
{"declarations":1,"errors":1,"key":"core.model.nml","ok":false,"target":"core.model.nml","type":"result","verb":"check","warnings":0}
{"closure":"complete","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

**Fix:** match the declaration (`#key("host")` vs `#live`).

## NML5002

**Contradictory directives.** *(A schema source under a package whose
vocabulary declares both — every front end.)* `#live` and `#restart` on
the same field contradict — a field has one reload class. The row sits
on the later of the two, the addition that made the contradiction.

```text transcript=tests/fixtures/directive-conflict
$ nml check --root . core.model.nml
core.model.nml:3:28: error[NML5002]: '#live' and '#restart' contradict — pick one
for more information, run: nml explain NML5002
error: 1 error(s)
```

**Fix:** keep exactly one.

## NML5003

**Undeclared sibling schema (advisory).** *(A schema source, every front
end.)* A schema source (`.model.nml` or `.schema.nml` — the two spellings
the kernel admits) sits beside a package's sources but is not declared in
the manifest's `[]schema` list, so it does not participate in validation:
an `info` row, the file itself still `ok`, exit 0.

```text transcript=tests/fixtures/directive-sibling
$ nml check --root . extra.schema.nml
extra.schema.nml:1:1: info[NML5003]: not part of package 'demo'; add a []schema entry to participate
extra.schema.nml: ok (1 declaration(s))
```

**Fix:** add a `[]schema` entry — or move the file if it is not meant to be
part of the package.

## NML5004

**Unknown template namespace.** *(Editor/project surface.)* A
`{{namespace.key}}` expression uses a namespace the project does not
configure (`templateNamespaces` in `nml-project.nml`). Comes with a
did-you-mean.

**Fix:** apply the suggestion, or add the namespace to the project config.
