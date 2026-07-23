# 1 · Your first NML file

You write Skylight's first config file, learn the four things every NML file
is made of — declarations, properties, nested blocks, comments — and check,
inspect, and format it with the CLI. This is the whole file you'll have by
the end:

```nml check
// Skylight — a hosted status-page service.
// This file grows with every chapter of the tutorial.

service Api:
    host = "0.0.0.0"
    port = 8080
    publicUrl = "https://status.skylight.dev"
    tags = ["web", "api"]

    database:
        url = "postgres://localhost/skylight"
        poolSize = 10
```

If you haven't installed the CLI yet, see [Before you start](README.md#before-you-start).

## Declarations

Create a file called `app.nml` and write the smallest possible version:

```nml check
service Api:
    port = 8080
```

The first line is a **declaration**: a keyword (`service`), a name (`Api`),
and a colon. Everything indented underneath is its **body**.

`service` is not a reserved word — NML has no fixed vocabulary of block
keywords. Your application decides what kinds of blocks exist (`service`,
`database`, `workflow`, `pipeline`…); the parser accepts any identifier. In
Chapter 3 you'll give keywords meaning by attaching schemas to them.

## Properties

A property is `name = value`. Values you've met so far are strings and
numbers; booleans and lists work the way you'd guess:

```nml check
service Api:
    host = "0.0.0.0"
    port = 8080
    debug = false
    tags = ["web", "api"]
```

Strings are always double-quoted. Numbers are exact — `port` is `8080`, not
`8080.000001` (NML numbers have integer semantics; you'll meet the full type
system in Chapter 2).

## Nested blocks

A name followed by a colon — with no `=` — opens a nested block. Skylight
needs a database:

```nml check
service Api:
    host = "0.0.0.0"
    port = 8080

    database:
        url = "postgres://localhost/skylight"
        poolSize = 10
```

`=` assigns a value; `:` opens a body. That's the entire distinction, and it
holds everywhere in the language.

## Comments

Comments start with `//` and run to the end of the line:

```nml check
// Skylight — a hosted status-page service.
service Api:
    port = 8080  // the load balancer forwards :443 here
```

(`#` is *not* a comment — it marks directives, a schema feature you'll meet
in Chapter 8.)

## Indentation is structure

Like Python, NML uses indentation instead of braces. The formatter's
canonical style is **4 spaces per level**. Tabs are rejected outright:

```nml check expect-error='tabs are not permitted'
service Api:
	host = "0.0.0.0"
```

```text
app.nml:2:1: error: tabs are not permitted in indentation; use spaces
```

That diagnostic is real — every error message in this tutorial is the actual
CLI output, verified in CI.

## Check, inspect, format

Assemble the full file from the top of the page into `app.nml`, then let the
tooling loop begin:

```bash
nml check app.nml
```

```text
app.nml: ok (1 declaration(s))
```

`nml check` is the command you'll use constantly: it parses, validates, and —
once you have a schema — type-checks. It exits non-zero on any error, so it
drops straight into CI.

`nml parse` dumps the parsed structure as JSON. Try it and look at one
property:

```bash
nml parse app.nml
```

```json
{
  "kind": {
    "Property": {
      "name": { "name": "port", "span": { "start": 65, "end": 69 } },
      "value": { "value": { "Number": 8080 }, "span": { "start": 72, "end": 76 } }
    }
  }
}
```

Every node carries its **span** — exact byte offsets into your file. That's
why NML diagnostics point at precise locations instead of whole lines, and
it's what you'll consume programmatically in Chapter 7.

`nml fmt app.nml` rewrites the file in canonical style (4-space indents,
normalized spacing) and preserves your comments. Run it whenever; it's
idempotent.

## Break it

Deliberately breaking things teaches you to read diagnostics before you need
them in anger. Remove the quotes from `host`:

```nml check expect-error='invalid number'
service Api:
    host = 0.0.0.0
```

```text
app.nml:2:12: error: invalid number: "0.0.0.0"
```

An unquoted `0.0.0.0` isn't a string — the parser tries to read it as a
number and tells you exactly where it gave up. Put the quotes back and
`nml check` is green again.

## Exercises

1. Skylight will cache rendered status pages. Add a `cache` nested block to
   `service Api` with `enabled = true` and `maxEntries = 500`, and make
   `nml check` pass.

   <details><summary>Solution</summary>

   ```nml check
   service Api:
       host = "0.0.0.0"
       port = 8080

       cache:
           enabled = true
           maxEntries = 500
   ```

   </details>

2. Mis-indent a line on purpose (dedent `port` by one space), run
   `nml check`, and read the error. Then run `nml fmt` — does it fix it?

   <details><summary>Solution</summary>

   You get a parse error pointing at the misaligned line (something like
   `unexpected token in block body`). `nml fmt` does **not** fix it —
   formatting is for valid files; indentation errors are structural, so you
   fix them by hand. The editor extension flags them as you type.

   </details>

## Common mistakes

- **Tabs.** `error: tabs are not permitted in indentation; use spaces` —
  configure your editor to insert spaces for `.nml` files.
- **Forgetting the colon.** `service Api` without `:` produces a cascade of
  `expected a declaration` errors starting on the *next* line — when you see
  that cascade, look one line up.
- **`=` vs `:`.** `database = ...` assigns a value; `database:` opens a
  block. If you wrote `=` and then indented a body under it, the parser
  rejects the body.

## Recap

- A file is a list of declarations: `keyword Name:` + an indented body of
  `name = value` properties, nested `name:` blocks, and `//` comments.
- Keywords are yours to invent — schemas (Chapter 3) give them meaning.
- `nml check` is your feedback loop; `nml parse` shows the structure with
  byte-exact spans; `nml fmt` keeps style canonical and comments intact.

Next: [Chapter 2 — Types that mean something](02-types-that-mean-something.md),
where Skylight gains prices, timeouts, and its first secret.
