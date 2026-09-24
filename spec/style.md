# NML Canonical Style

**Status:** Normative. This document defines what `nml fmt` writes.

`nml fmt` has no options. There is one canonical style, it is the one
described here, and `nml fmt --check` is the gate that holds a repository to
it — including this one: every `.nml` file this repository tracks is checked
on every documentation run.

## 1. What the formatter owns

The formatter owns **layout**: indentation, the line breaks the grammar
requires, the blank run between two lines, and the spacing between tokens on
a line. It owns **spelling** only where this specification already makes a
spelling normative (§5).

It does not own **structure** and it does not own **meaning**. It never
reorders a document, never adds or removes an entry, never restructures a
type expression, and never changes a value. Semantic rewrites belong to
`nml fix`.

Adopting the style on an existing tree is one command and one commit (§9).

Between those two lies a third category: choices the grammar leaves free and
a reader can read something into — a blank line that separates two groups, a
value the author put on its own line, an aligned column. **Those belong to
the author, and the formatter preserves them.** This is the rule behind §2,
§4 and §5, and it is the same rule `gofmt` applies to blank lines and
composite literals, and `zig fmt` to the trailing comma.

## 2. Blank lines

A blank line is the author's paragraph break, so the formatter keeps it:

- A run of blank lines the author wrote is preserved, **capped at two
  between top-level declarations and one inside a block**.
- A blank line **immediately after a block header** is removed. It separates
  a header from its own contents, which is nothing from nothing.
- A file never begins with a blank line, and never ends with one: the last
  line is followed by exactly one line terminator.
- The formatter never *inserts* a blank line. Two declarations the author
  wrote adjacent stay adjacent.

```
// two top-level groups, the author's spacing kept
const MaxRetries = 3
const Timeout = 30s

service Api:
    host = "0.0.0.0"
    port = 8080

    database:
        url = "postgres://localhost/app"
```

## 3. Comments

Every comment survives formatting, in its original position relative to the
code around it. A comment cannot be reordered, dropped, or moved across a
blank line: it is a token in the same stream as the code.

- A comment that **opens a line** stays an own-line comment, indented with
  the block it is written inside — including a comment that closes a block,
  which stays at the block's depth rather than drifting out to the next
  declaration.
- A comment that **follows code** on its line stays there.

Trailing whitespace inside a comment is removed, as it is everywhere else
(§4); nothing else about a comment's text changes.

## 4. Within a line

Indentation is four spaces per level, always regenerated: a two-space file
becomes a four-space file, and trailing whitespace is removed.

Between tokens the formatter writes the canonical spacing:

```
model service:
    host string
    port number(min = 1, max = 65535)
    regions set<string>?
    contact (string | []string)?
    tags []string #live #key("host")

service Api is monitored:
    port = $ENV.PORT | 8080
    gate = @role/admin & @role/ops
    limits = [1, 2, 3]
    retry = 1h30m
    price = 19.99 USD
```

Two columns are the **author's**, and the formatter neither invents them nor
destroys them:

- the gap before an arm's `->`
- the gap before a comment that follows code

```
// an aligned run stays aligned, byte for byte
oneof notifier by kind:
    "log"     -> notifierLog
    "email"   -> notifierEmail
    "webhook" -> notifierWebhook

// an unaligned run keeps its single space
landing:
    @role/admin -> "ops"
    else -> "status"
```

The formatter does not align anything on its own. Mandatory alignment
couples lines that have nothing to do with each other: adding one arm with a
long selector would rewrite every line around it, and the diff would blame
the author of the new arm for all of them. `rustfmt`, `prettier`, `black`,
`zig fmt`, `taplo` and `yamlfmt` all refuse alignment for this reason. What
they do not do, and what this rule adds, is preserve an alignment the author
chose.

**The cost of that rule, stated plainly: an alignment the formatter does
not maintain is one it will not repair.** Add an arm whose selector is
wider than the column, and the table is ragged — and stays ragged, because
each line keeps the gap its author wrote:

```
// after adding one longer arm, this is what `nml fmt` leaves — and what
// `nml fmt --check` calls canonical
oneof notifier by kind:
    "log"     -> notifierLog
    "email"   -> notifierEmail
    "webhook" -> notifierWebhook
    "pagerduty" -> notifierPager
```

Re-aligning is an edit, and edits are the author's. A formatter that did it
for you would be the mandatory alignment this section refuses, arriving one
run later. If a column matters to your team, widen it by hand in the same
commit that widens the table; if it does not, the single space is canonical
too.

## 5. Values and spellings

A value's text is written as the author wrote it, with these exceptions,
each of which this specification already makes normative:

| Written | Formatted | Why |
|---|---|---|
| `10_000` | `10000` | digit separators are spelling, never value ([syntax](syntax.md#number-literals)) |
| `007` | `7` | the same rule |
| `1h 30m`, `30m1h` | `1h30m` | the canonical duration form is attached, coarse to fine ([syntax](syntax.md#duration-literals)) |
| `19.99USD` | `19.99 USD` | a currency code is a noun, not a suffix ([syntax](syntax.md#money-literals)) |
| a raw edge space in a `"""` body | `\s` | so neither re-parsing nor an editor's trim-on-save can change the value ([syntax](syntax.md#string-literals)) |

A written scale is significant and survives: `2.50` stays `2.50`. A duration
is never rescaled: `72h` stays `72h`.

Where the grammar admits a value on the line of its `=` or `:` **or on the
next line**, the author's choice stands. The formatter never joins and never
splits:

```
// both forms are canonical: the specification teaches both
system = """
    You are an intent classifier.
    """

const LongPrompt =
    """
    You are an intent classifier.
    """
```

A multi-line string's body is indented to sit under its opening `"""` — one
level in from the line the `"""` ends, or at that line's own indentation
when the `"""` is alone on it. The closing delimiter aligns with the body,
which is [NML0020](../crates/nml-core/assets/error-index.md#nml0020)'s rule.

## 6. Transport

- The file's line terminator is preserved: a CRLF file stays a CRLF file.
- A byte-order mark is removed.
- The output is independent of terminal width. There is no line-length
  limit, and no construct in the grammar can be broken across lines to meet
  one.

## 7. Invalid files

`nml fmt` refuses a document that does not parse and validate, and writes
nothing. A tree recovered from errors is a guess at the author's intent, and
writing a guess back over their file is how a formatter loses work. `gofmt`
and `rustfmt` refuse for the same reason. Fix the errors — `nml check` names
them, and `nml fix` repairs the machine-applicable ones — then format.

## 8. Guarantees

For any document `nml fmt` accepts:

1. **The output parses.** The formatter can never write a document the
   parser rejects.
2. **Formatting is idempotent.** Formatting twice equals formatting once.
3. **The document is preserved.** The lowered tree of the output equals the
   input's, spans aside.
4. **Every significant token survives**, in order, with its text unchanged
   except for the normalizations in §5.
5. **Every comment survives**, in order, with its text unchanged (§3).
6. **No string or template literal's value changes.**
7. **Nothing is duplicated.** Output size is bounded by input size plus the
   indentation written, which follows from 4 and 5: the output's content is
   the input's tokens and comments, and nothing else.
8. **Line endings are stable** (§6): formatting a CRLF file and formatting
   its LF twin differ only in their terminators.

Guarantees 1 to 3 are checked by the `format` fuzz target on arbitrary
input. Guarantees 4 to 8 are checked by property tests in `nml-fmt`, over a
battery that covers every grammar shape and every layout this document
names.

## 9. Adopting the style

There are no options, so adopting it on an existing tree is one command
and one commit:

```
nml fmt --root . .              # write the whole tree in canonical style
nml fmt --check --root . .      # from then on, in CI
```

Format and commit BEFORE the gate goes on, and read that first diff — it
is the only large one there will be. Everything in it is layout (§1):
indentation regenerated, trailing whitespace dropped, a blank line after a
block header removed, a spelling §5 makes normative. No line in it is a
change of structure or of a value, which is what makes the diff safe to
approve in bulk. Afterwards `nml fmt` is a no-op on a file nobody edited,
so `--check` fails only on what the last commit touched.
