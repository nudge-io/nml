# 3 · Give it a schema

`service` and `plan` stop being arbitrary words. You write Skylight's schema
— models, an enum, defaults, optional fields — and watch the validator catch
a typo'd enum value, a missing field, a wrong type, and a credential that
almost got committed. The config file itself doesn't change this chapter;
it gains a type.

Where Chapter 2 landed: [`examples/02/app.nml`](examples/02/app.nml).
This chapter adds [`examples/03/skylight.model.nml`](examples/03/skylight.model.nml)
beside it.

## Models type your keywords

A schema lives in a `.model.nml` file. A `model` declaration types every
block whose keyword matches its name — `model service` types `service Api:`
(and `service Anything:`). Each field is `name type`:

```nml check
model database:
    url string
    poolSize number = 10
```

Two rules you already care about:

- **Fields are required by default.** A `database` block without a `url` is
  an error. Config is the place where forgetting a field should fail loudly.
- **`= value` declares a default.** Omit `poolSize` and the schema supplies
  `10` — your Rust code (Chapter 7) receives it as if it had been written.

Opt *out* of required with `?`:

```nml fragment
banner string?
```

## Enums

Skylight's log level should be one of four strings, not whatever a tired
operator types at 2 a.m.:

```nml check
enum logLevel:
    - "debug"
    - "info"
    - "warn"
    - "error"
```

A field then uses the enum as its type: `logLevel logLevel = "info"`.

## The full schema

Assemble `skylight.model.nml`:

```nml check
enum logLevel:
    - "debug"
    - "info"
    - "warn"
    - "error"

model database:
    url string
    poolSize number = 10

model service:
    host string
    port number
    publicUrl string
    logLevel logLevel = "info"
    requestTimeout duration = "30s"
    retries number = 3
    banner string?
    welcome string?
    tags []string
    apiKey secret
    database database

model plan:
    name string
    monthlyPrice money
    annualPrice money?
    trialDays number = 14
```

Read the field types you haven't met as declarations yet:

- `[]string` — a list of strings (`tags`). Any type can be listed: `[]plan`,
  `[]number`.
- `database database` — a **model reference**: the nested `database:` block
  is validated against `model database`, recursively.
- `duration`, `money`, `secret` — the Chapter 2 value types, now enforced.
  `requestTimeout = 30` or a literal string in `apiKey` are now errors, not
  surprises.
- Unions exist too: `(secret | string)` accepts either — the explicit
  opt-out if you truly want to allow literal tokens in a field (you usually
  don't; see below).

`nml validate skylight.model.nml` checks the schema itself is well-formed.

## Run the validator

`--schema` points at a **directory**; every `.model.nml` file in it is
loaded. Keep the schema next to the config (or in a `schemas/` directory —
your call):

```bash
nml check --schema . app.nml
```

```text
app.nml: ok (4 declaration(s))
```

The instance file from Chapter 2 passes unchanged — defaults cover
`logLevel`, `poolSize`, and `trialDays`, the fallback chain on `port`
resolves later at runtime, and `apiKey` is a proper reference chain.

## Read the diagnostics

Now break things, one at a time. Typo the enum value:

```nml check schema=docs/tutorial/examples/03 expect-error='did you mean "warn"'
service Api:
    host = "0.0.0.0"
    port = 8080
    publicUrl = "https://status.skylight.dev"
    logLevel = "wran"
    tags = []
    apiKey = $ENV.SKYLIGHT_API_KEY
    database:
        url = "postgres://localhost/skylight"
```

```text
app.nml:5:16: error[NML2000]: invalid value "wran" for 'logLevel': expected one of "debug", "info", "warn", "error" (did you mean "warn"?)
for more information, run: nml explain NML2000
```

Anatomy of an NML diagnostic: `file:line:col` pointing at the exact span,
a stable code in brackets, what was expected, and — when the fix is
machine-guessable — a did-you-mean (in the editor it's a one-click quick
fix). `nml explain NML2000` prints the code's full documentation offline.

Delete the `host` line:

```text
app.nml:1:9: error[NML2007]: missing required field 'host' (defined in model 'service')
```

Quote the port (`port = "8080"`):

```text
app.nml:3:12: error[NML2008]: type mismatch for 'port': expected number, got string
```

And the one from Chapter 2's warning — try a literal dev fallback on the
secret:

```nml check schema=docs/tutorial/examples/03 expect-error='[NML2006]'
service Api:
    host = "0.0.0.0"
    port = 8080
    publicUrl = "https://status.skylight.dev"
    tags = []
    apiKey = $ENV.SKYLIGHT_API_KEY | "dev-key"
    database:
        url = "postgres://localhost/skylight"
```

```text
app.nml:6:37: error[NML2006]: type mismatch for 'apiKey': expected environment variable ($ENV.VARIABLE_NAME), got string
```

A `secret` field is reference-only, deliberately: a literal leg would put a
credential in the committed file *and* silently mask a missing variable in
production. Chain a second reference (`$ENV.KEY | $ENV.KEY_DEV`), keep dev
values in your resolver (Chapter 7), or — if you genuinely mean it — type
the field `(secret | string)`.

## Strict mode: catch the typo'd *property*

Misspell a property name — `hots` instead of `host` — and lenient validation
lets it through, because unknown properties might belong to a tool it
doesn't know about:

```nml check schema=docs/tutorial/examples/03 expect-output='warning[NML2001]'
service Api:
    host = "0.0.0.0"
    hots = "oops"
    port = 8080
    publicUrl = "https://status.skylight.dev"
    tags = []
    apiKey = $ENV.SKYLIGHT_API_KEY
    database:
        url = "postgres://localhost/skylight"
```

```text
app.nml:3:5: warning[NML2001]: unknown property 'hots' (not defined in model 'service') (did you mean "host"?)
app.nml: ok (1 declaration(s))
```

A *warning*, and the file still passes — exit code 0. In CI you want the
hard line: `--strict` turns unknown properties and keywords into errors:

```bash
nml check --schema . --strict app.nml   # exit 1: error[NML2001] unknown property 'hots'
```

Rule of thumb: humans iterate leniently, CI runs `--strict`.

## Exercises

1. Chapter 1's exercise added a `cache` nested block (`enabled`,
   `maxEntries`). Write `model cache` for it — `enabled` defaulting to
   `true`, `maxEntries` required — and reference it from `model service` as
   an *optional* field.

   <details><summary>Solution</summary>

   ```nml check
   model cache:
       enabled bool = true
       maxEntries number

   model service:
       host string
       cache cache?
   ```

   (In the real schema you'd add the `cache cache?` line to the existing
   `model service` rather than redeclaring it.)

   </details>

2. Run `nml check --schema . --strict app.nml`, then add a deliberately
   misspelled property and confirm the exit code flips from 0 to 1. Use
   `nml explain NML2001` to read the code's full entry.

   <details><summary>Solution</summary>

   Lenient: `warning[NML2001] ... did you mean "host"?` and exit 0.
   With `--strict` the same finding is `error[NML2001]` and exit 1 — that's
   the CI gate.

   </details>

## Common mistakes

- **Forgetting `--schema`.** Plain `nml check` only parses; if nothing
  points at your models, nothing is validated against them.
- **Model name ≠ keyword.** `model services` will never match `service Api:`
  — the model's name must equal the block keyword exactly.
- **Marker on the wrong side.** Optional is `string?`, not `?string`; the
  marker trails the type.

## Recap

- `model <keyword>` types every matching block; fields are `name type`,
  required by default, `?` for optional, `= value` for defaults the consumer
  receives as real values.
- Enums, lists (`[]T`), model references, unions (`(A | B)`), and the typed
  primitives (`duration`, `money`, `secret`) make wrong configs fail at
  check time with span-precise, coded, did-you-mean diagnostics.
- `nml check --schema <dir>` is the loop; add `--strict` in CI so unknown
  names are errors, not warnings.

Next: [Chapter 4 — Compose and reuse](04-compose-and-reuse.md): Skylight
adds monitored endpoints in multiple regions, and the schema learns traits,
shared properties, sets, and the positional marker.
