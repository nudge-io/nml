# NML Models, Traits, and Enums

## Overview

Models define the structure of configuration objects. Once a model is defined, its
name becomes a keyword for declaring instances. Traits provide reusable field groups.
Enums restrict a field to a set of allowed values.

## Model Definitions

### Syntax

```
model modelName:
    fieldName fieldType
    fieldName fieldType = defaultValue
    fieldName fieldType?
```

### Field Presence Rules

- **No modifier** -- field is required. Instances must provide it.
- **`?`** -- field is optional. Instances may omit it.
- **`= value`** -- field has a default. Instances may omit it; the default is used.

```
model webProfile:
    siteName string                // required
    debug logLevel?                // optional
    sessionDuration duration = 24h  // has default
```

### Inline Nested Objects

For one-off nested structures, define them inline within the model:

```
model accessControl:
    sessionDuration duration = 24h

    urlRoutes:
        homeRoute path = "/"
        postLoginRoute path
        postLogoutRoute path
```

`urlRoutes` is an anonymous nested object. It does not create a reusable type.
For reuse, extract it into its own model.

### Shared Properties (`.` prefix)

The `.` prefix defines a property inherited by the list items of the body
it appears in — each body is its own scope, at any nesting depth (see
[syntax.md](syntax.md) §Shared Properties for the scope and precedence
rules):

```
model endpoint:
    address string

    .healthCheck:
        path path
```

When used in a `[]endpoint` array, `.healthCheck` applies to every element
unless overridden by a specific element.

### Positional Field (`+`)

The `+` marker after a field's type identifies which field receives the value
when a list item is a bare scalar. A model may declare at most one positional
field; `?+` marks an optional positional field:

```nml check
model resource:
    path path+
    method httpMethod = "GET"

enum httpMethod:
    - "GET"
    - "POST"
```

This means:
```
[]resource resources:
    - "/test/matt"           // fills the positional field: path = "/test/matt"
    - HomePage:
        path = "/"           // explicit form
```

## Trait Definitions

Traits define reusable groups of fields that models (and other traits)
mix in with `is`. A trait is **not instantiable**: it never types a block
keyword, never appears as a field type, and is never a `oneof` arm target
— it exists only to compose (RFC 0011).

### Syntax

```
trait traitName:
    fieldName fieldType
    ...
```

### Usage

A model mixes traits in after `is`:

```nml check
trait accessControlled:
    |allow []role
    |deny []role

model resource is accessControlled:
    path path+
    method httpMethod = "GET"

enum httpMethod:
    - "GET"
    - "POST"
```

The composed form is equivalent to declaring the trait's fields directly:

```
model resource:
    |allow []role
    |deny []role
    path path+
    method httpMethod = "GET"
```

Multiple traits are comma-separated after `is`:

```nml check
trait accessControlled:
    |allow []role
trait auditable:
    auditLog string?

model myThing is accessControlled, auditable:
    name string
```

### Semantics

- Traits share the model **namespace**: a trait and a model may not use
  the same name (NML2009), and every `is` target must resolve to a
  declared model or trait (NML2020, with a did-you-mean; an enum or
  `oneof` target is NML2021).
- Fields merge ancestor-first; a definition's own field overrides a
  same-named inherited one. Defaults, `?`/`+` markers, modifier fields,
  and `#directives` all travel with the merge. The single-positional rule
  (NML2011) applies to the *merged* field set.
- A trait cannot be instantiated (`monitored Probe:` is NML2024 — an
  error even in lenient validation), used as a field type (NML2022), or
  targeted by a `oneof` arm (NML2023).
- Traits may themselves compose (`trait a is b:`), including mixing in
  models; the `extends` graph is cycle-checked (NML2013).

## Enum Definitions

Enums restrict a field to a fixed set of string values.

### Syntax

```
enum enumName:
    - "value1"
    - "value2"
    - "value3"
```

### Usage

```
enum httpMethod:
    - "GET"
    - "POST"
    - "PUT"
    - "DELETE"
    - "PATCH"

model resource:
    method httpMethod = "GET"
```

An instance that uses a value not in the enum is a validation error.

## Models and Instance Declarations

Once a model is defined, its name becomes a keyword. Instance syntax is unchanged
from standard NML:

```
// Model definition
model service is accessControlled:
    localMount path
    resources []resource
    endpoints []endpoint

// Instance declaration
service NudgeService:
    |allow:
        - @role/admin
        - @public
    |deny = []
    localMount = "/"
    resources = registrationResources
    endpoints = registrationEndpoints
```

Models add validation. Existing NML files continue to work without models;
when models are present, the parser validates instances against them.

## File Conventions

- Model definitions: `*.model.nml` or `*.schema.nml`
- Instance declarations: named by purpose (e.g., `myapp.service.nml`)
- Models are loaded first from a known location, then instance files are
  validated against them
