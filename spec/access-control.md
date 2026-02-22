# NML Access Control

## Overview

NML has built-in syntax for access control via the `|` (pipe) modifier prefix.
The two core modifiers are `|allow` and `|deny`, which define which roles or
identities can access a resource, endpoint, or service.

## Syntax

### Inline Form

```
|allow = [@public, @role/admin]
|deny = []
```

### Block Form

```
|allow:
    - @role/admin
    - @public
    - @nudge:research/admin

|deny:
    - @nudge/admin/gotest
```

### Empty List

An empty block list is written as `[]`:

```
|deny = []
```

## Semantics

### `|allow`

Defines which roles are permitted access. If `|allow` is present, only the
listed roles have access. Roles not listed are implicitly denied.

### `|deny`

Defines which roles are explicitly denied access, even if they would otherwise
be allowed. `|deny` takes precedence over `|allow`.

### Evaluation Order

1. Check `|deny` -- if the identity matches any denied role, deny access.
2. Check `|allow` -- if the identity matches any allowed role, grant access.
3. If neither matches, deny access (closed by default).

### Inheritance

Access control modifiers are inherited from parent to child:

- A `service` defines top-level access rules
- Individual `resource` entries within the service can override or extend them
- A `[]resource` array can set access control on the array itself, applying to all items

```
service NudgeService:
    |allow:
        - @role/admin       // service-level allow

    resources:
        - PublicPage:
            |allow = [@public]   // overrides for this resource
            path = "/"

        - AdminPage:
            // inherits |allow = [@role/admin] from service
            path = "/admin"
```

## Built-in Role References

| Reference | Description |
|-----------|-------------|
| `@public` | Unauthenticated access (no login required) |
| `@private` | Authenticated access (login required) |
| `@anyone` | All access, authenticated or not |
| `@loggedIn` | Authenticated users (alias for `@private`) |
| `@admin` | Administrative access |

## User-Defined Role References

User-defined roles follow the `@namespace/path` pattern:

```
@role/admin
@role/pro-user
@nudge:research/admin
@nudge/{org}/admin/{dept}/update
@user/gmatty@gmail.com
@email/gmatty@gmail.com
```

### Namespacing

The `:` separator divides an organization prefix from a role path:

```
@nudge:research/admin
 ^^^^^  ^^^^^^^^^^^^^^^
 org    role path
```

The `/` separator divides path segments within the role:

```
@role/pro-user
 ^^^^  ^^^^^^^^
 namespace  role name
```

### Parameterized Roles

Role references can contain path variables that are matched at runtime:

```
@nudge/{org}/admin/{dept}/update
```

This matches any role where `{org}` and `{dept}` are filled in with actual values.

## Modeling Access Control

The `accessControlled` trait captures the `|allow` / `|deny` pattern for reuse
across models:

```
trait accessControlled:
    |allow []@roleRef
    |deny []@roleRef

model service (accessControlled):
    localMount path
    resources []resource
    endpoints []endpoint

model resource (accessControlled):
    path path <shorthand>
    method httpMethod = "GET"
```
