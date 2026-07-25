# Wire a custom secret resolver

`$ENV.KEY` references mean "externally resolved" — and **your resolver
decides the source**. `ValueResolver::env()` reads real environment
variables in production; any closure works: a vault client, a secrets
manager, a test fixture map. The closure receives the bare key
(`"API_KEY"`).

```nml check
service Api:
    apiKey = $ENV.API_KEY
    dbUrl = $ENV.DATABASE_URL | $ENV.DATABASE_URL_DEV
```

```rust source=docs/guides/examples/cookbook/examples/secret_resolver.rs
    let vault: HashMap<&str, &str> = [
        ("API_KEY", "sk-vault-1"),
        // DATABASE_URL is absent — the fallback chain's second leg resolves.
        ("DATABASE_URL_DEV", "postgres://localhost/dev"),
    ]
    .into();
    let resolver = ValueResolver::new(move |key| vault.get(key).map(|v| v.to_string()));

    let file = parse(source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().ok_or("no Api block")?;
    let config: Config = from_body_resolved(body, &resolver)?;
```

Full program: [`secret_resolver.rs`](examples/cookbook/examples/secret_resolver.rs)
— `cargo run -p nml-cookbook --example secret_resolver`.

**Security posture (by design):** a `secret`-typed schema field is
reference-only — it never holds a literal, so credentials cannot live in
committed config files, and a fallback chain's legs are references too
(`$ENV.A | $ENV.A_DEV`). Dev conveniences belong in the resolver (hand it a
fixture map in tests, as above), not in the file. `ValueResolver::without_env()`
is the fail-closed mode for contexts that must never read the process
environment.
