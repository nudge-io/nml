# Validate in CI

The CLI is the CI surface: parse + validate + schema check in one command,
non-zero exit on error, every finding coded.

```bash
# Parse + symbol checks + schema validation against a schema directory:
nml check --schema schemas/ deploy.nml

# Unknown properties/keywords become ERRORS (closed-world config):
nml check --schema schemas/ --strict deploy.nml
```

A config that omits a required field:

```nml check schema=docs/guides/examples/cookbook/ci expect-error='[NML2007]'
server Main:
    host = "0.0.0.0"
```

And a clean one:

```nml check schema=docs/guides/examples/cookbook/ci
server Main:
    host = "0.0.0.0"
    port = 8080
```

(These two blocks run against [`examples/cookbook/ci/`](examples/cookbook/ci/)
in this repo's CI — the failure *and* the success are both verified.)

**Exit codes are a stable interface** ([stability policy](../stability.md)):
`0` clean, non-zero on errors — wire it straight into CI. Warnings report
but don't fail; `--strict` upgrades unknown-name findings to errors. The
last line of an error run names the first code
(`for more information, run: nml explain NML2007`), and `nml explain
--list` enumerates every code your pipeline might meet.

**Choosing strictness:** run `--strict` on configs your own tool owns
end-to-end; leave it lenient for configs that downstream plugins may extend
(the [directive vocabulary recipe](directive-vocabulary.md) shows the
package-level way to keep even that closed).
