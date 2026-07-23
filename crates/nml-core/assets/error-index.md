# NML Error Index

Every diagnostic with a stable code has a section here, keyed `## NML0000`.
Codes are **stable from the first published release**: never renumbered,
never reused; a retired code keeps its section as a tombstone (see the
[stability policy](../stability.md)). This index is bidirectionally guarded
by `just docs-test`: a code without a section — or a section without a code —
fails CI, and most examples below run through the real CLI.

Bands (allocation convenience, not API): 0001–0999 lex/parse ·
1000–1999 symbols & resolution · 2000–2999 schema loading & validation ·
3000–3999 values & money · 4000–4999 packages & store · 5000–5999 editor/LSP.

## NML1000

**Duplicate declaration.** Two top-level declarations share one name; names
are a single namespace so references stay unambiguous. The second
declaration is flagged; the first wins downstream.

```nml check expect-error='[NML1000]'
service Api:
    port = 8080

service Api:
    port = 9090
```

**Fix:** rename or merge one of the declarations.

## NML1001

**Unresolved reference.** A value references a name no declaration defines.
Comes with a did-you-mean when a declared name is close.

```nml check expect-error='[NML1001]'
const DefaultPort = 8080

service Api:
    port = DefaultPrt
```

**Fix:** apply the suggestion (`DefaultPort`), or declare the missing name.

## NML1002

**Const/template reference cycle.** `const`/`template` chains must resolve to
a value; a cycle never does.

```nml check expect-error='[NML1002]'
const A = B
const B = A
```

**Fix:** break the cycle by giving one member a concrete value.

## NML2000

**Invalid enum value.** The value is not one of the enum's declared variants
(matching is exact; a near-miss gets a machine-applicable suggestion).

```nml check schema=docs/examples/readme expect-error='[NML2000]'
service Api:
    host = "0.0.0.0"
    port = 8080
    logLevel = "wran"
    apiKey = $ENV.API_KEY
```

**Fix:** apply the suggestion (`"warn"`), or use any declared variant.

## NML2001

**Unknown property.** The property is not defined by the governing model — a
warning by default (unknown data is skippable), an error under `--strict`.

```nml check schema=docs/examples/readme expect-output='[NML2001]'
service Api:
    host = "0.0.0.0"
    hots = "typo"
    port = 8080
    apiKey = $ENV.API_KEY
```

**Fix:** apply the suggestion, or add the field to the model.

## NML2002

**Unknown modifier.** The `|modifier` name is not in the configured modifier
set (project config or package profile). Surfaces wherever a modifier
vocabulary is configured; comes with a did-you-mean.

**Fix:** apply the suggestion (e.g. `|alow` → `|allow`), or register the
modifier.

## NML2003

**Unknown oneof discriminant.** The discriminator value matches no declared
arm of the `oneof`.

```nml check schema=docs/errors/schemas expect-error='[NML2003]'
email Notifier:
    kind = "postmrak"
```

**Fix:** apply the suggestion (`"postmark"`), or use any declared arm value.

## NML2004

**Unknown block keyword (strict).** Under `--strict`, a block keyword with no
model or `oneof` definition is an error instead of being silently skipped.

```nml check strict schema=docs/examples/readme expect-error='[NML2004]'
servce Api:
    port = 8080
```

**Fix:** apply the suggestion (`service`), or define the model.

## NML2005

**Unknown array keyword (strict).** The array's item keyword names no model
or `oneof`, and its items carry bodies that would go unvalidated.

```nml check strict schema=docs/examples/readme expect-error='[NML2005]'
[]widget Widgets:
    - first:
        size = 1
```

**Fix:** define the item model, or correct the keyword.

## NML2006

**Literal in a `secret` field.** `secret` fields hold *references*
(`$ENV.NAME`), never literal credential material — the file must never
contain the secret value (see the [stability policy](../stability.md)'s
security notes and the README's secrets section).

```nml check schema=docs/examples/readme expect-error='[NML2006]'
service Api:
    host = "0.0.0.0"
    port = 8080
    apiKey = "sk-live-credentials-in-git"
```

**Fix:** use `$ENV.API_KEY` (fallbacks may chain to other references:
`$ENV.API_KEY | $ENV.API_KEY_DEV`), or declare the field `(secret | string)`
if literals are genuinely intended.

## NML2007

**Missing required field.** Fields are required unless marked `?` or given a
default; the instance omits one.

```nml check schema=docs/examples/readme expect-error='[NML2007]'
service Api:
    port = 8080
    apiKey = $ENV.API_KEY
```

**Fix:** supply the field (`host`), mark it optional, or give it a default.

## NML2008

**Type mismatch.** The value's type does not match the field's declared type.

```nml check schema=docs/examples/readme expect-error='[NML2008]'
service Api:
    host = "0.0.0.0"
    port = "eight thousand"
    apiKey = $ENV.API_KEY
```

**Fix:** supply a value of the declared type (`port number`).

## NML2009

**Duplicate definition.** The same model/enum/oneof name is defined more than
once across a schema set; the first definition wins downstream.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2009]'
// a.model.nml and b.model.nml both define:
model widget:
    name string+
```

**Fix:** remove or rename one definition.

## NML2010

**Reserved type-constructor name.** `set` (live) and `map` (reserved) are
type constructors; a definition so named could never be referenced with
arguments.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2010]'
model set:
    x string?
```

**Fix:** rename the definition.

## NML2011

**Multiple positional fields.** A bare scalar list item supplies one value,
so a model may mark at most one field positional (`+`).

```nml check schema=docs/errors/schemas-bad expect-error='[NML2011]'
model twoPositional:
    a string+
    b string+
```

**Fix:** keep one `+`; the others become named properties.

## NML2012

**Oneof arm references an unknown model.** Every arm's target must be a
declared `model`.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2012]'
oneof thing by kind:
    "x" -> missingModel
```

**Fix:** declare the model, or correct the arm's target name.

## NML2015

**Duplicate discriminator value.** Each arm's value must be unique within
its `oneof` — dispatch would otherwise be ambiguous.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2015]'
oneof dupValue by kind:
    "x" -> widget
    "x" -> widget
```

**Fix:** give each arm a distinct value.

## NML2016

**Oneof name collision.** A `oneof` shares a name with a model or enum;
names are one namespace across all three definition kinds.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2016]'
oneof widget by kind:
    "w" -> widget
```

**Fix:** rename the union (or the colliding definition).

## NML2017

**Default discriminator matches no arm.** A declared default must name one
of the union's arm values.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2017]'
oneof withDefault by kind = "zzz":
    "a" -> widget
```

**Fix:** point the default at a declared arm value.

## NML2018

**Discriminator type is not an enum.** `by <field> as <type>` requires
`<type>` to be a declared enum.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2018]'
oneof badType by kind as notAnEnum:
    "a" -> widget
```

**Fix:** declare the enum, or drop the `as` clause.

## NML2019

**Enum-typed arms are not exhaustive.** With `as <enum>`, the arm values
must equal the enum's variants exactly — no missing variant, no arm outside
the enum. Both directions report this code.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2019]'
enum letters:
    - "a"
    - "b"
oneof exhaustive by kind as letters:
    "a" -> widget
```

**Fix:** add the missing arm (or remove the extra one).

## NML2013

**Inheritance cycle.** `is` chains must be acyclic.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2013]'
model cycleA is cycleB:
    x string?
model cycleB is cycleA:
    y string?
```

**Fix:** break the cycle; extract shared fields into a trait both compose.

## NML2014

**Model-reference cycle (advisory).** Model fields reference each other in a
loop. Legal — recursive configs exist — but often an unintended
self-reference, so it warns.

```nml check schema=docs/errors/schemas-bad expect-error='[NML2014]'
model node:
    next otherNode?
model otherNode:
    back node?
```

**Fix:** if intended, ignore (it is only a warning); otherwise break the
loop.

## NML3000

**Invalid money literal.** The amount is malformed for the currency — most
commonly more decimal places than the currency's ISO 4217 exponent allows
(money is exact minor units, never floats).

```nml check expect-error='[NML3000]'
product Widget:
    price = 19.999 USD
```

**Fix:** use the currency's precision (`19.99 USD`; JPY takes none: `1999 JPY`).

## NML3001

**Unknown currency code.** The trailing code is not in the ISO 4217 table;
near-misses get a machine-applicable suggestion.

```nml check expect-error='[NML3001]'
product Widget:
    price = 19.99 USE
```

**Fix:** apply the suggestion (`USD`), or use any ISO 4217 code.

## NML5000

**Unknown directive.** *(Editor/package surface.)* `#name` is not in the
covering schema package's directive vocabulary. Comes with a sigil-inclusive
did-you-mean (`#lvie` → `#live`).

**Fix:** apply the suggestion, or declare the directive in the package's
`[]directive` vocabulary.

## NML5001

**Directive arity mismatch.** *(Editor/package surface.)* The directive takes
no argument but was given one, or requires one and lacks it — per its
declared `arg` kind.

**Fix:** match the declaration (`#key("host")` vs `#live`).

## NML5002

**Contradictory directives.** *(Editor/package surface.)* `#live` and
`#restart` on the same field contradict — a field has one reload class.

**Fix:** keep exactly one.

## NML5003

**Undeclared sibling schema (advisory).** *(Editor/package surface.)* A
`.model.nml` file sits beside a package's sources but is not declared in the
manifest's `[]schema` list, so it does not participate in validation.

**Fix:** add a `[]schema` entry — or move the file if it is not meant to be
part of the package.

## NML5004

**Unknown template namespace.** *(Editor/project surface.)* A
`{{namespace.key}}` expression uses a namespace the project does not
configure (`templateNamespaces` in `nml-project.nml`). Comes with a
did-you-mean.

**Fix:** apply the suggestion, or add the namespace to the project config.
