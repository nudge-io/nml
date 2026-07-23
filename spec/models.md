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
    fieldName fieldType <constraint, ...>
```

### Field Presence Rules

- **No modifier** -- field is required. Instances must provide it.
- **`?`** -- field is optional. Instances may omit it.
- **`= value`** -- field has a default. Instances may omit it; the default is used.

```
model webProfile:
    siteName string                // required
    debug logLevel?                // optional
    sessionDuration duration = "24h"  // has default
```

### Constraints

Constraints are specified in angle brackets after the type:

| Constraint | Applies to | Description |
|------------|-----------|-------------|
| `<unique>` | any | Value must be unique across all instances |
| `<secret>` | string | Masked in logs, supports `$ENV.X` resolution |
| `<token>` | string | Used as a lookup identifier |
| `<distinct>` | lists | All items must be unique |
| `<integer>` | number | Must be a whole number |
| `<min = N>` | number | Minimum value (inclusive) |
| `<max = N>` | number | Maximum value (inclusive) |
| `<minLength = N>` | string | Minimum string length |
| `<maxLength = N>` | string | Maximum string length |
| `<pattern = "re">` | string | Must match the regex pattern |
| `<currency = "X">` | money | Restrict accepted ISO 4217 codes |

Multiple constraints are comma-separated:

```
port number <integer, min = 1, max = 65535>
```

### Inline Nested Objects

For one-off nested structures, define them inline within the model:

```
model accessControl:
    sessionDuration duration = "24h"

    urlRoutes:
        homeRoute path = "/"
        postLoginRoute path
        postLogoutRoute path
```

`urlRoutes` is an anonymous nested object. It does not create a reusable type.
For reuse, extract it into its own model.

### Shared Properties (`.` prefix)

The `.` prefix defines a property inherited by all children of a list:

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

Traits define reusable groups of fields that can be mixed into models.

### Syntax

```
trait traitName:
    fieldName fieldType
    ...
```

### Usage

A model includes traits by listing them in parentheses:

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
model service (accessControlled):
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
