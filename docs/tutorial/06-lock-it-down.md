# 6 · Lock it down

A status page is public by definition — except the admin console, the ops
dashboard, and the parts you'd rather not explain in an incident review.
This chapter adds access control to Skylight: `|allow`/`|deny` modifiers,
declared roles with members, and a role-routed landing page built on routing
arms.

Where this chapter lands: [`examples/06/app.nml`](examples/06/app.nml) and
[`examples/06/skylight.model.nml`](examples/06/skylight.model.nml).

## Modifiers

Access-control properties wear a `|` sigil, which keeps them visually and
semantically apart from your data fields:

```nml fragment
service Api:
    |allow = [@public]
    |deny = []
```

`|allow` lists the roles permitted — unlisted roles are implicitly denied.
`|deny` explicitly revokes, and takes precedence over `|allow`. Both take
role references, inline (`[@a, @b]`) or in block form:

```nml fragment
|allow:
    - @role/admin
    - @public
```

`@public` is a built-in role (unauthenticated access); so are `@private`
(authenticated) and `@admin`. Your own roles live under `@role/<name>`.

## Declaring modifiers in the schema

Like everything else in NML, modifiers are typed — by **modifier fields**,
declared with the same sigil. Skylight puts them in a trait, because
services and endpoints both need them:

```nml fragment
trait accessControlled:
    |allow []role?
    |deny []role?

model endpoint is monitored, accessControlled:
    url string+
```

`role` is a primitive type: a field typed `role` (or `[]role`) holds role
*references*. Note `endpoint` now mixes in **two** traits — Chapter 4's
composition scaling exactly as promised. The modifier fields are optional
(`[]role?`): a block that says nothing inherits its parent's rules.

Now the admin console endpoint can protect itself, overriding the service's
public default — access control flows parent to child, and the nearest
declaration wins:

```nml check schema=docs/tutorial/examples/06
[]endpoint monitoredEndpoints:
    - Api:
        url = "https://api.skylight.dev"

    - AdminConsole:
        url = "https://admin.skylight.dev"
        |allow = [@role/admin]
```

## Roles with members

`@role/admin` names a role; a `role` declaration *defines* it — label,
description, members:

```nml check schema=docs/tutorial/examples/06
role admin:
    label = "Skylight operators"
    members:
        - @user/ops@skylight.dev
```

Members are themselves role references — `@user/<email>` for individuals,
and role references can nest (a role's members can include another role).
How references resolve to real identities is your application's business;
the schema guarantees the *shape* is right.

## Routing arms: `(role -> V)`

Skylight's landing page should differ by who's looking: operators get the
ops dashboard, everyone else the public status page. That's not access
control — it's **routing by role** — and it uses the arm syntax you met in
`oneof`, now as a field type:

```nml fragment
model service is accessControlled:
    landing (string | (role -> string))?
```

`(role -> string)` is a typed arm map: the field's body is an ordered,
first-match list of `@selector -> target` arms, with `else` always legal
as the final catch-all:

```nml fragment
service Api:
    landing:
        @role/admin -> "ops"
        else -> "status"
```

The union with `string` is Chapter 5's body-shape dispatch doing real work:
a config that doesn't care about per-role routing writes
`landing = "status"` and never sees the arm machinery.

Two properties worth knowing:

- **First match wins.** Arms are checked top to bottom; order is meaning.
- **Targets are references for *your* application.** The validator types
  them (here: `string`) but doesn't resolve `"ops"` to anything — like
  template namespaces, the meaning belongs to the host app.

Put a normal property inside an arms body and the validator draws the line
precisely:

```text
app.nml:27:9: error: expected a routing arm ('@selector -> Target' or 'else -> Target'); this field is typed '(role -> …)' and holds only arms
```

## Bring it together

The full chapter state — role declaration, service-level `|allow`, the
protected endpoint, and the routed landing — is
[`examples/06/app.nml`](examples/06/app.nml); `nml check --schema .
app.nml` reports `ok (7 declaration(s))`.

## Exercises

1. Add a `viewer` role for Skylight's support team
   (`@user/support@skylight.dev`) and give the `AdminConsole` endpoint
   read access to it alongside admins.

   <details><summary>Solution</summary>

   ```nml check schema=docs/tutorial/examples/06
   role viewer:
       label = "Read-only operators"
       members:
           - @user/support@skylight.dev

   []endpoint monitoredEndpoints:
       - AdminConsole:
           url = "https://admin.skylight.dev"
           |allow = [@role/admin, @role/viewer]
   ```

   </details>

2. Route `@role/viewer` to a `"readonly"` landing, keeping admins on
   `"ops"` and everyone else on `"status"`. Which arm order is correct if a
   user could hold both roles and should get `"ops"`?

   <details><summary>Solution</summary>

   ```nml fragment
   landing:
       @role/admin  -> "ops"
       @role/viewer -> "readonly"
       else         -> "status"
   ```

   `@role/admin` must come first — arms are first-match, so the more
   privileged route has to precede the broader one.

   </details>

## Common mistakes

- **Trusting `|deny = []` to mean "deny nothing" implicitly.** It does —
  but write it anyway on security-relevant blocks; an explicit empty deny
  list documents intent the next reader can't misread.
- **Mixing routing into access control.** `|allow` decides *whether* a
  request proceeds; `(role -> V)` arms decide *what it gets*. Keeping them
  separate keeps both auditable.
- **Forgetting `else` on a routing field.** Arms are first-match; without a
  catch-all, roles that match nothing get no route — decide explicitly what
  "everyone else" sees.

## Recap

- `|allow`/`|deny` modifiers carry role references; deny wins; rules flow
  parent to child with child overrides — and modifier *fields* (`|allow
  []role`) make them schema-checked like everything else.
- `role` declarations define labeled, membered roles; `@role/<name>`,
  `@user/<email>`, and built-ins like `@public` reference them.
- `(role -> V)` typed arm maps route by role, first-match, with `else` as
  the catch-all — unioned with a scalar, simple configs stay simple.

Next: [Chapter 7 — Embed it in Rust](07-embed-it-in-rust.md) — the chapter
where the config stops being a file you check and becomes data your program
trusts.
