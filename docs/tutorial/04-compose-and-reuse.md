# 4 · Compose and reuse

Skylight starts monitoring endpoints — the actual point of a status page —
and both files learn to stop repeating themselves: traits compose on
the schema side, shared properties de-duplicate list items on the config
side, `set<T>` enforces uniqueness, and the positional marker `+` makes the
common case one line.

Where this chapter lands: [`examples/04/app.nml`](examples/04/app.nml) and
[`examples/04/skylight.model.nml`](examples/04/skylight.model.nml).

## Array declarations and references

Endpoints are a list, and lists can be declared at the top level with
`[]<model> <Name>:` — then referenced by name where they're used:

```nml fragment
[]endpoint monitoredEndpoints:
    - Api:
        url = "https://api.skylight.dev"
    - Marketing:
        url = "https://www.skylight.dev"

service Api:
    endpoints = monitoredEndpoints
```

`- Api:` is a **named list item** — the name is part of the data (in
Chapter 7 it arrives in Rust as a `name` field). `endpoints =
monitoredEndpoints` is a **reference assignment**: the list lives once,
independent of the block that uses it, which keeps a growing file
navigable.

## Traits: schema-side reuse

Every endpoint gets health-checked on an interval, with a timeout. Those
fields belong together, and other models will want them later. That's a
`trait` — a reusable bundle of fields a model mixes in with `is`:

```nml check
trait monitored:
    checkInterval duration = "60s"
    timeout duration = "5s"

model endpoint is monitored:
    url string+
    healthPath path = "/health"
    regions set<string>?
```

`model endpoint is monitored` gives `endpoint` all of `monitored`'s fields,
defaults included; the model's own fields override same-named inherited
ones. A model can mix in several (`model x is a, b:`), and traits can
compose other traits. What a trait can *never* do is stand alone:
`monitored Probe:` as a block is an error (`NML2024`) — a trait declares a
capability, not a block type, and the validator holds that line for you.
Typo an `is` target and you get a did-you-mean (`NML2020`); traits keep
schemas honest the same way models keep configs honest.

(Ignore the `+` on `url string+` for a moment; it's the last trick this
chapter teaches.)

## Shared properties: config-side reuse

Skylight wants a 10-second timeout on *these* endpoints — stricter than the
trait's `5s` default, but the same for every item in the list. Instead of
repeating it per item, a `.name = value` entry declares a **shared
property** every list item inherits:

```nml fragment
[]endpoint monitoredEndpoints:
    .timeout = "10s"

    - Api:
        url = "https://api.skylight.dev"
    - Marketing:
        url = "https://www.skylight.dev"
```

An item that sets its own `timeout` overrides the shared value — shared
properties are defaults, one level closer than the schema's. Nested-block
form works too (`.healthCheck:` with a body).

The precedence ladder so far, weakest to strongest: schema default → shared
property → the item's own value.

## `set<T>`: collections where identity matters

`regions set<string>?` types the regions an endpoint is monitored from. A
`set` differs from a list (`[]T`) in two ways: element order carries no
meaning, and duplicates are errors —

```nml check schema=docs/tutorial/examples/04 expect-error='duplicate set element'
[]endpoint eps:
    - Api:
        url = "https://api.skylight.dev"
        regions = ["us-east", "us-east"]
```

```text
app.nml:4:31: error: duplicate set element for 'regions' 'us-east' — set elements must be unique
```

Use `set<T>` whenever "the same element twice" is a config bug — regions,
feature flags, hostnames. (In Chapter 8 you'll see the payoff compound:
set changes diff as *element added/removed*, not as *the list changed*.)

## The positional marker `+`

Most of Skylight's endpoints need nothing but a URL. The schema already
says so:

```nml fragment
model endpoint is monitored:
    url string+
```

The `+` marks `url` as the model's **positional** field — the one field a
bare list item fills. So the docs site doesn't need a name and a body:

```nml fragment
[]endpoint monitoredEndpoints:
    - Api:
        url = "https://api.skylight.dev"
        regions = ["us-east", "eu-west"]

    - "https://docs.skylight.dev"
```

The bare string item is a complete endpoint: `url` from the item itself,
`healthPath`/`checkInterval` from defaults, `timeout` from the shared
property. One line for the common case, full block form when you need more —
and both validate against the same model. A model gets at most one
positional field; declare two and schema loading fails with `NML2011`.

## Bring it together

The full chapter state is [`examples/04/app.nml`](examples/04/app.nml)
against [`examples/04/skylight.model.nml`](examples/04/skylight.model.nml) —
`nml check --schema . app.nml` reports `ok (5 declaration(s))`.

## Exercises

1. Add a `StatusApi` endpoint at `https://api.skylight.dev/status`
   monitored from `us-east` only, keeping the shared timeout. Then give it
   its own `timeout = "2s"` and convince yourself the override wins (then
   try `timeout = 2` and read the type error).

   <details><summary>Solution</summary>

   ```nml check schema=docs/tutorial/examples/04
   []endpoint monitoredEndpoints:
       .timeout = "10s"

       - StatusApi:
           url = "https://api.skylight.dev/status"
           regions = ["us-east"]
           timeout = "2s"
   ```

   `timeout = 2` fails with `error[NML2008]: type mismatch for 'timeout':
   expected duration, got number`.

   </details>

2. Skylight's `tags` shouldn't repeat either. Change its type in the schema
   so `tags = ["web", "web"]` becomes an error, and check the fixture still
   passes.

   <details><summary>Solution</summary>

   In `model service`, change `tags []string` to `tags set<string>`. The
   fixture's `["web", "api"]` has no duplicates, so it still validates;
   `["web", "web"]` now fails with `duplicate set element`.

   </details>

## Common mistakes

- **Two positional fields.** One `+` per model — a second one fails schema
  loading with `error[NML2011]: model declares more than one scalar
  shorthand field`.
- **Shared property typos.** `.timeout` only helps if the model has a
  `timeout` field — a misspelled shared property is simply an unknown
  property on every item (run `--strict` to make that loud).
- **Reaching for `[]T` out of habit.** If duplicates are a bug, say so in
  the type: `set<T>` turns "reviewer vigilance" into a diagnostic.

## Recap

- Reuse has a home on each side: traits + `is` compose schemas;
  `.name = value` shared properties de-duplicate list items, with item
  values overriding shared, and shared overriding schema defaults.
- Top-level `[]model Name:` array declarations plus reference assignment
  (`endpoints = monitoredEndpoints`) keep large configs modular.
- `set<T>` makes uniqueness a type; the single `+` positional field makes
  the common item a one-liner that still validates fully.

Next: [Chapter 5 — One of many](05-one-of-many.md): alerts that go to
different notifiers — discriminated unions with `oneof`.
