# 2 · Types that mean something

Skylight gains prices, a request timeout, a secret, and reusable constants —
and you meet the value types that make NML more than strings-and-numbers:
money that can't lose a cent, durations, secret *references*, fallback
chains, and templates.

Where Chapter 1 left off: [`examples/01/app.nml`](examples/01/app.nml).
Where this chapter lands: [`examples/02/app.nml`](examples/02/app.nml).

## The nine primitives

NML has nine primitive types: `string`, `number`, `bool`, `money`,
`duration`, `path`, `secret`, `object`, and `role`. You've used the first
three. A few of them have dedicated literal syntax you can use right now;
the rest come into play when a schema types your fields (Chapter 3) and when
you meet access control (`role`, Chapter 6) and embedded domain bodies
(`object`, Chapter 8's schema).

Numbers are **exact** — every `number` is an exact decimal, never a binary
float, so `taxRate = 0.20` is 0.20 forever (not 0.2000000000000000111…),
integers survive to 34 significant digits, and the written precision
(`2.50` vs `2.5`) is preserved. Which raises the question: how do you
write `$29.99`?

## Money

You *could* write it exactly as a plain number — exactness isn't the
problem. What money adds is **currency rules**: a money literal pairs the
exact decimal with an ISO 4217 currency code, is stored as integer minor
units — `29.99 USD` is 2999 cents, forever — and the currency's real
decimal conventions are enforced at parse time:

```nml check
plan Pro:
    name = "Professional"
    monthlyPrice = 29.99 USD
    annualPrice = 299.99 USD
    trialDays = 30
```

That's a second top-level declaration — a file holds as many as you need,
and `nml check` counts them for you (`ok (2 declaration(s))`).

The currency's real decimal rules are enforced at parse time. USD has two
minor digits:

```nml check expect-error='USD has 2 decimal places'
plan Broken:
    monthlyPrice = 19.999 USD
```

```text
app.nml:2:20: error[NML3002]: invalid money value: USD has 2 decimal places, but got 3 in "19.999"
```

Japanese yen has zero, so `1299 JPY` is valid and `12.99 JPY` is not. Typo
the code itself and you get a suggestion:

```nml check expect-error='unknown currency code'
plan Broken:
    monthlyPrice = 19.99 USE
```

```text
app.nml:2:20: error[NML3001]: invalid money value: unknown currency code: USE (did you mean "USD"?)
for more information, run: nml explain NML3001
```

That trailing line appears under any coded diagnostic — `nml explain
NML3001` prints the full error-index entry for the code, offline.

## Durations

Durations are literals — an integer with a unit suffix attached: `h`,
`m`, `s`, `ms`, `us`, or `ns`:

```nml check
service Api:
    requestTimeout = 30s
    cacheTtl = 15m
```

They are typed quantities, not strings: `30s` equals `30000ms` wherever
values compare, the magnitude is a whole number (`30.5s` is an error with
a fix — `30500ms`), and a mistyped unit like `30x` is rejected with a
suggestion. When a schema types the field as `duration` (next chapter), a
bare number, a boolean, or a quoted string there becomes a check-time
error instead of a surprise at 3 a.m.

## Secrets are references, not values

Skylight's API needs a signing key. You do **not** paste it into the file —
a `secret` value is a *reference* that names where the real value lives:

```nml check
service Api:
    apiKey = $ENV.SKYLIGHT_API_KEY | $ENV.SKYLIGHT_API_KEY_DEV
```

`$ENV.SKYLIGHT_API_KEY` means "resolve from the environment at runtime". The
`|` is a **fallback chain**: try the primary, then the next reference. Note
that *both* legs are references — the committed file never contains a
credential, so a leaked repo never leaks a key. When your app embeds NML
(Chapter 7), a pluggable resolver decides what references mean — env vars,
a vault, your own lookup.

You might be tempted to write a literal dev default — `$ENV.KEY |
"dev-key"`. For a field a schema declares `secret`, that is a **validation
error by design** (you'll see it fire in Chapter 3): a literal leg would put
a credential in the committed file and silently mask a missing variable in
production. Keep dev values in the resolver, or chain a second reference as
above.

Fallbacks aren't only for secrets — any value can carry a chain:

```nml check
service Api:
    port = $ENV.PORT | 8080
```

## Constants and multiline strings

`const` declares a file-level reusable value; reference it by bare name.
Long text uses triple-quoted multiline strings, dedented automatically:

```nml check
const MaxRetries = 3
const StatusBanner =
    """
    All systems operational.
    Subscribe for updates at status.skylight.dev.
    """

service Api:
    retries = MaxRetries
    banner = StatusBanner
```

Line endings never change what a string means: a file saved with Windows
CRLF endings produces byte-identical values to the same file saved with
LF, and characters that can't appear raw in source (control characters,
for instance) are written as escapes like `\u{1B}` — the
[Source text](../../spec/syntax.md#source-text) section of the spec has
the full contract.

## Templates

Strings can interpolate `{{namespace.key}}` expressions. They are *not*
expanded at parse time — they're preserved in the structure and resolved by
the host application at runtime, which decides which namespaces exist:

```nml check
service Api:
    welcome = "Welcome to {{config.appName}} — status you can trust."
```

Double braces distinguish templates from single-brace path placeholders like
`/user/{id}` — those you'll meet with the `path` type in Chapter 4.

## Bring it together

Merge everything into `app.nml` — constants at the top, the new values on
`service Api`, the `plan Pro` block after it. The result is
[`examples/02/app.nml`](examples/02/app.nml); `nml check` reports
`ok (4 declaration(s))` (two constants, two blocks).

## Exercises

1. Skylight is launching in Japan. Add a `plan ProTokyo` block priced at
   `4400 JPY` monthly with a 14-day trial. Then try `44.00 JPY` and read the
   diagnostic before fixing it.

   <details><summary>Solution</summary>

   ```nml check
   plan ProTokyo:
       name = "Professional (JP)"
       monthlyPrice = 4400 JPY
       trialDays = 14
   ```

   `44.00 JPY` fails with `error[NML3002]: invalid money value: JPY has 0
   decimal places, but got 2 in "44.00"`.

   </details>

2. Add a `statusWebhookSecret` to `service Api` that resolves from
   `$ENV.SKYLIGHT_WEBHOOK_SECRET` and falls back to a *dev environment
   reference* (not a literal).

   <details><summary>Solution</summary>

   ```nml check
   service Api:
       statusWebhookSecret = $ENV.SKYLIGHT_WEBHOOK_SECRET | $ENV.SKYLIGHT_WEBHOOK_SECRET_DEV
   ```

   </details>

## Common mistakes

- **Floats for prices.** `monthlyPrice = 29.99` is just a number; write
  `29.99 USD` and precision is guaranteed by the currency's own rules
  (NML3002 catches wrong decimals, NML3001 catches bad codes).
- **Literal fallbacks on secrets.** `$ENV.KEY | "dev-key"` — a schema-typed
  `secret` field rejects this (NML2006). Chain references instead.
- **Single braces in templates.** `{config.appName}` is a path placeholder,
  not a template; templates need `{{...}}`.

## Recap

- Money is exact minor units with per-currency decimal rules; durations are
  unit-suffixed values; both are parse-checked today and schema-typed
  tomorrow.
- Secrets are references resolved at runtime; fallback chains use `|`, and
  secret chains stay reference-only end to end.
- `const` + bare-name references remove repetition; `{{...}}` templates
  defer to the host app.

Next: [Chapter 3 — Give it a schema](03-give-it-a-schema.md), where `service`
and `plan` stop being arbitrary words and start being types.
