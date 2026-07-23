# 5 · One of many

When an endpoint goes down, Skylight has to tell someone — in development
that's a log line, in production an email to on-call or a webhook into the
paging system. Three differently-shaped configs, one concept. That's a
discriminated union, and `oneof` is its schema form.

Where this chapter lands: [`examples/05/app.nml`](examples/05/app.nml) and
[`examples/05/skylight.model.nml`](examples/05/skylight.model.nml).

## Three shapes, one type

Model each notifier shape as its own model, then bind them into a `oneof`
whose **discriminator** field (`kind`) selects the variant:

```nml check
model notifierLog:
    level logLevel = "warn"

model notifierEmail:
    from string
    to string
    serverToken secret

model notifierWebhook:
    url string
    signingSecret secret?

enum logLevel:
    - "debug"
    - "info"
    - "warn"
    - "error"

oneof notifier by kind = "log":
    "log"     -> notifierLog
    "email"   -> notifierEmail
    "webhook" -> notifierWebhook
```

Each arm binds a discriminator value to a variant model. The discriminator
is owned by the union — variant models don't redeclare `kind`. The
`= "log"` after `by kind` is a **default arm**: omit `kind` in an instance
and the log variant is selected.

A `oneof` goes anywhere a model goes: block keyword, field type
(`notifier notifier?`), or list element (`[]notifier`) — which is what
Skylight needs:

```nml fragment
model service:
    // …existing fields…
    notifiers []notifier
```

## Writing the instances

The discriminator sits flat alongside the chosen variant's fields:

```nml check schema=docs/tutorial/examples/05
[]notifier alertNotifiers:
    - Console:
        level = "error"

    - Oncall:
        kind = "email"
        from = "alerts@skylight.dev"
        to = "oncall@skylight.dev"
        serverToken = $ENV.SKYLIGHT_POSTMARK_TOKEN

    - PagerBridge:
        kind = "webhook"
        url = "https://hooks.pager.example/skylight"
```

`Console` never says `kind = "log"` — the default arm covers it, and the
variant's own defaults (`level = "warn"`) would cover the rest, so a fully
defaulted notifier could be an empty block.

## Validation is per-variant

The validator resolves `kind` first, then enforces **that variant's**
contract. Point `kind` at nothing and the union itself answers:

```nml check schema=docs/tutorial/examples/05 expect-error='[NML2003]'
[]notifier alertNotifiers:
    - Oncall:
        kind = "slack"
        from = "alerts@skylight.dev"
```

```text
app.nml:3:16: error[NML2003]: unknown kind "slack" for oneof 'notifier'; expected one of: "log", "email", "webhook"
```

Forget a field the *selected* variant requires, and the diagnostic names the
variant model, not just the union:

```nml check schema=docs/tutorial/examples/05 expect-error="missing required field 'serverToken'"
[]notifier alertNotifiers:
    - Oncall:
        kind = "email"
        from = "alerts@skylight.dev"
        to = "oncall@skylight.dev"
```

```text
app.nml:2:7: error[NML2007]: missing required field 'serverToken' (defined in model 'notifierEmail')
```

And a field from the *wrong* variant — `serverToken` on a log notifier — is
an unknown property for `notifierLog` (a warning in lenient mode, an error
under `--strict`, like every unknown property).

## Exhaustive unions

When the set of kinds matters beyond one field, declare it as an enum and
type the discriminator with `as`:

```nml check
model notifierLog:
    quiet bool = false

model notifierEmail:
    to string

enum notifierKind:
    - "log"
    - "email"

oneof notifier by kind as notifierKind = "log":
    "log"   -> notifierLog
    "email" -> notifierEmail
```

Now the arms must cover the enum **exactly** — add a kind to the enum and
forget its arm (or vice versa), and *schema loading* fails. The clauses
compose left to right: `by <field> [as <enum>] [= <default>]`.

## Dispatch by shape

`oneof` dispatches on a value. Union field types dispatch on **shape** — you
met `(secret | string)` in Chapter 3. Skylight's maintenance contact can be
one address or several:

```nml fragment
maintenanceContact (string | []string)?
```

```nml fragment
maintenanceContact = "oncall@skylight.dev"
// …or…
maintenanceContact = ["oncall@skylight.dev", "ops@skylight.dev"]
```

Both validate; a number doesn't:

```text
app.nml:25:26: error: type mismatch for 'maintenanceContact': expected one of string, []string; got number
```

Rule of thumb: `oneof` when the variants are *named alternatives with their
own fields*; a union type when one field reasonably takes *several value
shapes*.

## Exercises

1. Add a `Backup` webhook notifier pointing at
   `https://hooks.backup.example/skylight` that signs its payloads with
   `$ENV.SKYLIGHT_BACKUP_HOOK_SECRET`.

   <details><summary>Solution</summary>

   ```nml check schema=docs/tutorial/examples/05
   []notifier alertNotifiers:
       - Backup:
           kind = "webhook"
           url = "https://hooks.backup.example/skylight"
           signingSecret = $ENV.SKYLIGHT_BACKUP_HOOK_SECRET
   ```

   </details>

2. Convert the chapter's `oneof` to the enum-typed form (`as notifierKind`),
   then add `- "sms"` to the enum *without* adding an arm and watch schema
   loading fail. Read the diagnostic, then decide which side to fix.

   <details><summary>Solution</summary>

   Declare `enum notifierKind` with the three kinds and write
   `oneof notifier by kind as notifierKind = "log":`. Adding `"sms"` to the
   enum alone fails at load with an exhaustiveness error — either add
   `"sms" -> notifierSms` (and the model) or remove the enum value. The
   point: the union can't silently drift from the enum.

   </details>

## Common mistakes

- **Redeclaring the discriminator in a variant.** `kind` belongs to the
  `oneof`; a variant model declaring its own `kind` field is a schema error.
- **Relying on the default arm you didn't declare.** Without `= "log"` in
  the `by` clause, omitting `kind` is a missing-discriminator error — the
  default is opt-in.
- **Using `oneof` where a union type does.** If the alternatives are just
  "one or many" or "reference or literal", `(A | B)` on the field is
  lighter than a union of models.

## Recap

- `oneof <name> by <field>:` binds discriminator values to variant models
  with `"value" -> model` arms; instances carry the discriminator flat and
  are validated against the *selected* variant, span-precisely.
- `by <field> as <enum> = "default"` adds exhaustiveness (arms ↔ enum,
  checked at load) and a default arm for the omitted-discriminator case.
- Union field types (`(string | []string)`) dispatch on value shape —
  lighter-weight polymorphism for a single field.

Next: [Chapter 6 — Lock it down](06-lock-it-down.md): who may *use* all
this — roles, `|allow`/`|deny`, and routing arms.
