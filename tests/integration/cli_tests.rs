use std::path::Path;
use std::process::Command;

fn nml_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nml"));
    // Integration tests run from the nml-cli dir; set cwd to workspace root
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    cmd.current_dir(workspace_root);
    // The pins compare the sentences' Unicode spelling: hermetic under
    // any locale the developer's or CI's shell carries (r88 P6).
    cmd.env("NML_UNICODE", "1");
    cmd
}

/// Run `nml` from the repo root under the given environment — the
/// harness's own `NML_UNICODE` pin lifted, each `(name, None)` removed,
/// each `(name, Some(v))` set: `(exit code, stdout, stderr)`.
fn run_env(vars: &[(&str, Option<&str>)], args: &[&str]) -> (i32, String, String) {
    let mut cmd = nml_bin();
    cmd.env_remove("NML_UNICODE");
    for (name, value) in vars {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
    let out = cmd.args(args).output().expect("failed to run nml");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn test_parse_valid_service() {
    let output = nml_bin()
        .args(["parse", "tests/fixtures/valid/minimal-service.nml"])
        .output()
        .expect("failed to run nml");

    assert!(output.status.success(), "parse should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"declarations\""));
    assert!(stdout.contains("MinimalService"));
}

#[test]
fn test_check_valid_files() {
    let files = [
        "tests/fixtures/valid/minimal-service.nml",
        "tests/fixtures/valid/full-service.nml",
        "tests/fixtures/valid/role-templates.nml",
        "tests/fixtures/valid/web-server.nml",
        "tests/fixtures/valid/pricing.nml",
        "tests/fixtures/valid/scalar-shared-property.nml",
        "tests/fixtures/valid/number-boundaries.nml",
        "tests/fixtures/valid/numeric-facets.nml",
        "tests/fixtures/valid/duration-compound.nml",
    ];

    for file in files {
        let output = nml_bin()
            .args(["check", file])
            .output()
            .expect("failed to run nml");

        assert!(
            output.status.success(),
            "check should succeed for {file}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// RFC 0016: numbers outside the exact decimal domain are NML0014 with
/// the structured reason; trailing-dot literals are NML0013 with the
/// remove-the-dot suggestion.
#[test]
fn test_check_number_boundaries() {
    let cases = [
        (
            "tests/fixtures/invalid/number-too-many-digits.nml",
            "NML0014",
            "35 significant digits",
        ),
        (
            "tests/fixtures/invalid/number-trailing-dot.nml",
            "NML0013",
            "decimal point",
        ),
    ];
    for (file, code, needle) in cases {
        let output = nml_bin()
            .args(["check", file])
            .output()
            .expect("failed to run nml");
        assert!(!output.status.success(), "check should fail for {file}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(code), "{file}: expected {code} in {stderr}");
        assert!(
            stderr.contains(needle),
            "{file}: expected {needle:?} in {stderr}"
        );
    }
}

#[test]
fn test_check_duplicate_detection() {
    let output = nml_bin()
        .args(["check", "tests/fixtures/invalid/duplicate-role.nml"])
        .output()
        .expect("failed to run nml");

    assert!(!output.status.success(), "check should fail for duplicates");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate"));
}

#[test]
fn test_help() {
    let output = nml_bin()
        .args(["help"])
        .output()
        .expect("failed to run nml");

    assert!(output.status.success());
    // Help is output, not an error (r69a B1): stdout, exit 0; stderr silent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("USAGE"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn test_version() {
    let output = nml_bin()
        .args(["version"])
        .output()
        .expect("failed to run nml");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nml 0.1.0"));
}

#[test]
fn test_parse_money_values() {
    let output = nml_bin()
        .args(["parse", "tests/fixtures/valid/money-values.nml"])
        .output()
        .expect("failed to run nml");

    assert!(
        output.status.success(),
        "parse should succeed for money values"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FreePlan"));
    assert!(stdout.contains("ProPlan"));
    assert!(stdout.contains("JapanPlan"));
    assert!(stdout.contains("Money"));
    assert!(stdout.contains("USD"));
    assert!(stdout.contains("JPY"));
}

#[test]
fn test_parse_duration_values() {
    // The duration wire shape is an API (RFC 0017 §6), pinned exactly:
    // externally tagged, segments array of {magnitude, unit} pairs.
    let output = nml_bin()
        .args(["parse", "tests/fixtures/valid/duration-values.nml"])
        .output()
        .expect("failed to run nml");

    assert!(
        output.status.success(),
        "parse should succeed for duration values"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let value = &json["declarations"][0]["kind"]["Block"]["body"]["entries"][0]["kind"]["Property"]
        ["value"]["value"];
    assert_eq!(
        value,
        &serde_json::json!({"Duration": {"segments": [{"magnitude": 30, "unit": "s"}]}}),
        "wire shape drifted: {value}"
    );
    for unit in ["\"s\"", "\"ms\"", "\"h\"", "\"m\"", "\"us\"", "\"ns\""] {
        assert!(stdout.contains(unit), "missing unit {unit}");
    }
    // Separators are spelling: the wire carries the value, bare.
    assert!(stdout.contains("\"magnitude\": 1000"), "{stdout}");
}

#[test]
fn test_parse_compound_duration_values() {
    // Compound literals (RFC 0017 §10) ride the same wire shape: one
    // segments array, canonical coarse→fine order, pinned exactly.
    let output = nml_bin()
        .args(["parse", "tests/fixtures/valid/duration-compound.nml"])
        .output()
        .expect("failed to run nml");

    assert!(
        output.status.success(),
        "parse should succeed for compound durations: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let entries = &json["declarations"][0]["kind"]["Block"]["body"]["entries"];
    let value_of = |i: usize| &entries[i]["kind"]["Property"]["value"]["value"];
    assert_eq!(
        value_of(0),
        &serde_json::json!({"Duration": {"segments": [
            {"magnitude": 1, "unit": "h"},
            {"magnitude": 30, "unit": "m"}
        ]}}),
        "compound wire shape drifted"
    );
    assert_eq!(
        value_of(1),
        &serde_json::json!({"Duration": {"segments": [
            {"magnitude": 5, "unit": "m"},
            {"magnitude": 2, "unit": "s"}
        ]}}),
    );
    // The authored single-unit respelling of the same value is stored
    // faithfully — never re-segmented on the wire.
    assert_eq!(
        value_of(2),
        &serde_json::json!({"Duration": {"segments": [
            {"magnitude": 90, "unit": "m"}
        ]}}),
    );
}

#[test]
fn test_parse_secret_values() {
    let output = nml_bin()
        .args(["parse", "tests/fixtures/valid/secret-values.nml"])
        .output()
        .expect("failed to run nml");

    assert!(
        output.status.success(),
        "parse should succeed for secret values"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Postmark"));
    assert!(stdout.contains("Stripe"));
    assert!(stdout.contains("Secret"));
    assert!(stdout.contains("POSTMARK_SERVER_TOKEN"));
    assert!(stdout.contains("STRIPE_API_KEY"));
    assert!(stdout.contains("STRIPE_WEBHOOK_SECRET"));
}

#[test]
fn test_check_bad_money_precision() {
    let output = nml_bin()
        .args(["check", "tests/fixtures/invalid/bad-money-precision.nml"])
        .output()
        .expect("failed to run nml");

    assert!(
        !output.status.success(),
        "check should fail for bad money precision"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The exact classified finding — an any-of-("…"|"error") assertion
    // was vacuously true for every failing check.
    assert!(
        stderr.contains("NML3002") && stderr.contains("2 decimal places, but got 3"),
        "the precision finding is classified and exact: {stderr}"
    );
}

#[test]
fn test_check_money_and_secret_valid_files() {
    let files = [
        "tests/fixtures/valid/money-values.nml",
        "tests/fixtures/valid/secret-values.nml",
    ];

    for file in files {
        let output = nml_bin()
            .args(["check", file])
            .output()
            .expect("failed to run nml");

        assert!(
            output.status.success(),
            "check should succeed for {file}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_check_schema_dir_accepts_schema_extension_and_inheritance() {
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "tests/fixtures/schema-check/schema",
            "tests/fixtures/schema-check/widget-ok.nml",
        ])
        .output()
        .expect("failed to run nml");

    assert!(
        output.status.success(),
        "check against .schema.nml dir should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The `.schema.nml` source was APPLIED, not skipped: an instance missing a
    // required field fails against the same directory (a listing that dropped
    // the spelling would validate it under nothing and exit 0).
    let (code, _, stderr) = run(&[
        "check",
        "--schema",
        "tests/fixtures/schema-check/schema",
        "tests/fixtures/schema-check/widget-missing-required.nml",
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error[NML"),
        "the .schema.nml source judged the instance: {stderr}"
    );
}

#[test]
fn test_check_schema_enforces_inherited_required_field() {
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "tests/fixtures/schema-check/schema",
            "tests/fixtures/schema-check/widget-missing-required.nml",
        ])
        .output()
        .expect("failed to run nml");

    assert!(
        !output.status.success(),
        "check should fail when an inherited required field is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `name` is filled by the block identifier (RFC 0005); `kind` is the inherited
    // required field the instance omits.
    assert!(
        stderr.contains("missing required field 'kind'"),
        "stderr should report the inherited field: {stderr}"
    );
}

#[test]
fn test_check_schema_reports_duplicate_model_names() {
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "tests/fixtures/schema-check/dup-schema",
            "tests/fixtures/schema-check/widget-ok.nml",
        ])
        .output()
        .expect("failed to run nml");

    assert!(
        !output.status.success(),
        "check should fail when schema files define duplicate models"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate model definition 'widget'"),
        "stderr should report the duplicate model: {stderr}"
    );
}

#[test]
fn test_fmt_produces_output() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = workspace_root.join("tests/fixtures/valid/minimal-service.nml");
    let temp = std::env::temp_dir().join(format!("nml_fmt_test_{}.nml", std::process::id()));
    std::fs::copy(&source, &temp).expect("failed to copy test file");

    let output = nml_bin()
        .args(["fmt", temp.to_str().unwrap()])
        .output()
        .expect("failed to run nml");

    assert!(output.status.success(), "fmt should succeed");

    let contents = std::fs::read_to_string(&temp).expect("failed to read formatted file");
    assert!(contents.contains("service MinimalService:"));
    assert!(contents.contains("localMount = \"/\""));

    std::fs::remove_file(&temp).ok();
}

/// `nml fix` (RFC 0017 §4.1) end to end: the duration migration
/// (`"30s"` → `30s`, including schema defaults) and the ledgered
/// `=>` → `->` fix apply in one invocation over a directory; the result
/// is idempotent and passes `check`; `--dry-run` prints a unified diff
/// and writes nothing.
#[test]
fn test_fix_applies_migrations_and_is_idempotent() {
    let dir = std::env::temp_dir().join(format!("nml_fix_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let app = dir.join("app.nml");
    std::fs::write(
        &app,
        "model job:\n    timeout duration\n    backoff duration = \"250ms\"\n\njob Nightly:\n    timeout = \"30s\"\n",
    )
    .expect("write");
    let legacy = dir.join("legacy.nml");
    std::fs::write(
        &legacy,
        "oneof email by kind:\n    \"log\" => emailLog\n\nmodel emailLog:\n    path string?\n",
    )
    .expect("write");

    // Dry-run: a diff, no writes.
    let output = nml_bin()
        .args(["fix", "--dry-run", dir.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("-    timeout = \"30s\""), "{stdout}");
    assert!(stdout.contains("+    timeout = 30s"), "{stdout}");
    assert!(stdout.contains("+    \"log\" -> emailLog"), "{stdout}");
    assert!(
        std::fs::read_to_string(&app).unwrap().contains("\"30s\""),
        "dry-run must not write"
    );

    // Apply: both files rewritten, then a second run finds nothing.
    let output = nml_bin()
        .args(["fix", dir.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "{output:?}");
    let fixed = std::fs::read_to_string(&app).unwrap();
    assert!(fixed.contains("timeout = 30s"), "{fixed}");
    assert!(fixed.contains("backoff duration = 250ms"), "{fixed}");
    assert!(
        std::fs::read_to_string(&legacy)
            .unwrap()
            .contains("\"log\" -> emailLog")
    );
    let output = nml_bin()
        .args(["fix", dir.to_str().unwrap()])
        .output()
        .expect("run nml");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 edit(s) applied"), "idempotent: {stdout}");

    // The fixed file passes check.
    let output = nml_bin()
        .args(["check", app.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "fixed file must check clean");

    std::fs::remove_dir_all(&dir).ok();
}

/// Atomic writes preserve the original's permission bits: a fixer rewrite
/// of a 0600 config must not silently widen it to the umask default.
#[cfg(unix)]
#[test]
fn test_fix_preserves_file_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("nml_fix_perms_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = dir.join("private.nml");
    std::fs::write(
        &file,
        "model job:\n    timeout duration\n\njob A:\n    timeout = \"30s\"\n",
    )
    .expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let output = nml_bin()
        .args(["fix", file.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "{output:?}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("timeout = 30s"),
        "fix applied"
    );
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "permissions must survive the rewrite");
    std::fs::remove_dir_all(&dir).ok();
}

/// The fixer's refusals: a value with no machine-applicable fix (the
/// deliberately-invalid duration fixture) is left byte-identical, and
/// unfixable diagnostics are reported as remaining.
#[test]
fn test_fix_never_touches_unfixable_files() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let fixture = workspace_root.join("tests/fixtures/invalid/bad-duration-default.model.nml");
    let before = std::fs::read_to_string(&fixture).unwrap();
    let output = nml_bin()
        .args(["fix", fixture.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
    assert!(
        stdout.contains("1 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture).unwrap(),
        before,
        "unfixable file must be untouched"
    );
}

#[test]
fn test_fmt_preserves_comments() {
    let temp = std::env::temp_dir().join("nml_fmt_comments_test.nml");
    std::fs::write(
        &temp,
        "// header comment\nservice App: // trailing\n    // body comment\n    port=8080 // why\n",
    )
    .expect("failed to write test file");

    let output = nml_bin()
        .args(["fmt", temp.to_str().unwrap()])
        .output()
        .expect("failed to run nml");

    assert!(output.status.success(), "fmt should succeed");

    let contents = std::fs::read_to_string(&temp).expect("failed to read formatted file");
    assert!(
        contents.contains("// header comment\n"),
        "header comment lost: {contents}"
    );
    assert!(
        contents.contains("service App: // trailing\n"),
        "trailing header comment lost: {contents}"
    );
    assert!(
        contents.contains("    // body comment\n"),
        "body comment lost: {contents}"
    );
    assert!(
        contents.contains("port = 8080 // why\n"),
        "trailing property comment lost (and spacing should normalize): {contents}"
    );

    std::fs::remove_file(&temp).ok();
}

#[test]
fn test_validate_runs_schema_finders_on_model_files() {
    // RFC 0011: `nml validate` of a schema file runs the loader's finder
    // pipeline — an unresolved `is` target is a coded error with a
    // did-you-mean, not a silent pass.
    let output = nml_bin()
        .args(["validate", "tests/fixtures/invalid/unknown-mixin.model.nml"])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success(), "unknown `is` target must fail");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("NML2020"), "{combined}");
    assert!(
        combined.contains("did you mean \"monitored\"?"),
        "{combined}"
    );
}

#[test]
fn test_check_rejects_trait_instantiation() {
    // RFC 0011: a trait keyword is an error even in lenient mode.
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "tests/fixtures/invalid/trait-instantiation",
            "tests/fixtures/invalid/trait-instantiation/app.nml",
        ])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("NML2024"), "{combined}");
    assert!(combined.contains("cannot be instantiated"), "{combined}");
}

#[test]
fn test_check_matches_validate_on_definition_files() {
    // `check` is a superset of `validate`: a definition file's composition
    // errors surface without --schema, once.
    let output = nml_bin()
        .args(["check", "tests/fixtures/invalid/unknown-mixin.model.nml"])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        combined.matches("NML2020").count(),
        2, // the finding + the `nml explain NML2020` hint line
        "exactly one finding and its explain hint; got:\n{combined}"
    );
}

#[test]
fn test_check_self_contained_trait_file_is_clean_against_foreign_schema() {
    // A file declaring both a trait and its composer must not be flagged
    // against an unrelated --schema directory (false NML2020 regression pin).
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "docs/errors/schemas",
            "tests/fixtures/invalid/trait-instantiation/s.model.nml",
        ])
        .output()
        .expect("failed to run nml");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "self-contained definitions resolve; got:\n{combined}"
    );
}

#[test]
fn test_self_contained_file_validates_with_no_flags() {
    // RFC 0012: `model cache` above `cache Foo:` types Foo — one file, no
    // --schema. Missing required field caught; fixed file passes.
    let dir = "tests/fixtures/schema-check";
    let output = nml_bin()
        .args(["check", &format!("{dir}/self-contained-bad.nml")])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("NML2007"), "{combined}");

    let output = nml_bin()
        .args(["check", &format!("{dir}/self-contained-good.nml")])
        .output()
        .expect("failed to run nml");
    assert!(output.status.success(), "fixed self-contained file passes");
}

#[test]
fn test_file_vs_schema_dir_collision_is_nml2009() {
    // RFC 0012: one namespace — a checked file redefining a directory
    // schema's name is a duplicate-definition error, never a silent shadow.
    let output = nml_bin()
        .args([
            "check",
            "--schema",
            "docs/errors/schemas",
            "tests/fixtures/schema-check/collides-with-dir.nml",
        ])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("NML2009"), "{combined}");
}

#[test]
fn test_validate_and_check_agree_on_definition_files() {
    // A bad schema default (the string "5x" in a duration field — not
    // duration text, so not even migratable to a literal) must fail BOTH
    // verbs with the same code — the definitions verbs can never disagree.
    // Since RFC 0017 a non-duration value in a duration field is the
    // ordinary type mismatch, and this fixture is a value `nml fix` must
    // never rewrite (no machine-applicable suggestion exists for it).
    let fixture = "tests/fixtures/invalid/bad-duration-default.model.nml";
    for verb in ["validate", "check"] {
        let output = nml_bin()
            .args([verb, fixture])
            .output()
            .expect("failed to run nml");
        assert!(!output.status.success(), "{verb} must fail");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("NML2008"), "{verb}: {combined}");
    }
}

#[test]
fn test_verbs_agree_on_type_shape_rules_too() {
    // RFC 0007 §4.3 shape rules run through the SAME body pass in both
    // verbs — the structural guarantee that the R1/R2 parity class is
    // closed for good.
    let rel = "tests/fixtures/invalid/arm-shape.model.nml";
    for verb in ["validate", "check"] {
        let output = nml_bin().args([verb, rel]).output().expect("run nml");
        assert!(!output.status.success(), "{verb} must fail");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("NML2033"), "{verb}: {combined}");
    }
}

#[test]
fn test_strict_with_nothing_to_enforce_is_a_usage_error() {
    // RFC 0012 follow-up: `--strict` with an empty schema universe fails
    // the invocation loudly instead of silently degrading to parse-only —
    // the "CI points at the wrong path and stays green" trap.
    let output = nml_bin()
        .args([
            "check",
            "--strict",
            "tests/fixtures/valid/minimal-service.nml",
        ])
        .output()
        .expect("failed to run nml");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("nothing to enforce"), "{combined}");
}

/// The same refusal in a CLOSED universe has a different cause and a
/// different remedy, and used to state neither: "no --schema directory
/// given and none declared in the file" was read beside a workspace whose
/// manifest declares schemas and validates its neighbours. The manifests
/// are there; they claim other files. So the sentence names the file, the
/// closure, the operator's remedy and the verb that shows the claim.
#[test]
fn strict_on_a_file_no_binding_claims_names_the_closure_and_the_remedy() {
    let fixture = fixture("workspace");
    let root = fixture.to_str().unwrap().to_string();
    let unclaimed = fixture.join("docs/unclaimed.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, "--strict", &unclaimed]);
    assert_eq!(
        code, 2,
        "a usage error, as in the open case: {stdout}{stderr}"
    );
    for want in [
        "--strict has nothing to enforce on docs/unclaimed.nml",
        "no binding claims it in the closed universe (2 manifest(s) discovered)",
        "add a `files` glob that claims it (an operator change), or drop --strict",
        "run `nml binding docs/unclaimed.nml` to see the claim",
    ] {
        assert!(stderr.contains(want), "want {want:?} in {stderr}");
    }
    // The open universe keeps the sentence that is true there.
    assert!(
        !stderr.contains("no --schema directory given"),
        "the open-context cause must not be stated in a closed one: {stderr}"
    );
}

// ── RFC 0019: layer composition through `nml check` ─────────────────────

fn check_fixture(file: &str) -> std::process::Output {
    nml_bin()
        .args(["check", file])
        .output()
        .expect("failed to run nml")
}

#[test]
fn layers_summary_example_checks_clean() {
    let out = check_fixture("tests/fixtures/layers/summary.nml");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn layers_pure_stack_assembly_checks_clean() {
    let out = check_fixture("tests/fixtures/layers/pure-stack-assembly.nml");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn layers_sealed_violation_is_nml2060_with_related_note() {
    let out = check_fixture("tests/fixtures/layers/sealed-violation.nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2060"), "{stderr}");
    assert!(stderr.contains("sealed here"), "{stderr}");
    assert!(stderr.contains("nml explain NML2060"), "{stderr}");
}

#[test]
fn layers_union_switch_seal_is_nml2060_end_to_end() {
    // The union face of the seal backstop, through the real CLI: names
    // the switch, the buried seal's full path, the teaching tail, and
    // the "sealed here" note.
    let out = check_fixture("tests/fixtures/layers/union-switch-seal.nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2060"), "{stderr}");
    assert!(
        stderr.contains("variant switch to `as cash` on 'payment'"),
        "{stderr}"
    );
    assert!(stderr.contains("payment.pan"), "{stderr}");
    assert!(
        stderr.contains("unseal the field in the schema"),
        "{stderr}"
    );
    assert!(stderr.contains("sealed here"), "{stderr}");
}

#[test]
fn layers_linearization_contradiction_is_nml2077() {
    let out = check_fixture("tests/fixtures/layers/linearization-contradiction.nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2077"), "{stderr}");
    // The teaching shape: the contradicting pair is NAMED, with the fix.
    assert!(
        stderr.contains("'base' is already a transitive base of 'mid'"),
        "{stderr}"
    );
    assert!(stderr.contains("list it before"), "{stderr}");
}

#[test]
fn layers_unmatched_item_is_nml2067_with_hint() {
    let out = check_fixture("tests/fixtures/layers/unmatched-item.nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2067"), "{stderr}");
    assert!(stderr.contains("submitSearch"), "did-you-mean: {stderr}");
}

#[test]
fn layers_structural_errors_fire_without_schema() {
    let out = check_fixture("tests/fixtures/layers/no-schema-structural.nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2059"), "{stderr}");
}

#[test]
fn layers_is_after_uses_is_a_loud_parse_error() {
    // Regression: `flow F uses base is T:` used to silently split into a
    // bodyless declaration plus a bogus `is T:` block that swallowed the
    // body — and `nml fmt` then canonicalized the corruption.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("is-after-uses.nml");
    std::fs::write(&f, "flow F uses base is T:\n    entrypoint = \"x\"\n").unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(!out.status.success(), "must not parse clean");
}

#[test]
fn validate_and_check_agree_on_merge_policy_errors() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("bad-policy.nml");
    std::fs::write(&f, "model m:\n    xs []string #identity\n").unwrap();
    for verb in ["validate", "check"] {
        let out = nml_bin()
            .args([verb, f.to_str().unwrap()])
            .output()
            .expect("failed to run nml");
        assert!(!out.status.success(), "{verb} must reject NML2068");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("NML2068"), "{verb}: {stderr}");
    }
}

#[test]
fn validate_and_check_agree_on_unresolved_uses_refs() {
    // `validate` does not compose, but its "unresolved references"
    // contract covers the header clause — same NML2059, same wording.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("unresolved-uses.nml");
    std::fs::write(&f, "flow t uses missingLayer:\n    entrypoint = \"x\"\n").unwrap();
    for verb in ["validate", "check"] {
        let out = nml_bin()
            .args([verb, f.to_str().unwrap()])
            .output()
            .expect("failed to run nml");
        assert!(!out.status.success(), "{verb} must reject the dangling ref");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("NML2059"), "{verb}: {stderr}");
        assert!(stderr.contains("does not resolve"), "{verb}: {stderr}");
    }
}

/// A `top` clause naming `n` bases (each a two-ref stack) — written to
/// `dir` and checked: the run must reject it (NML2066, the 16-layer cap)
/// BEFORE the C3 merge; the wall time of the run.
fn time_wide_clause_check(dir: &Path, n: usize) -> std::time::Duration {
    let f = dir.join(format!("wide-clause-{n}.nml"));
    let mut src = String::from(
        "model thing:\n    v string\n\nthing a:\n    v = \"a\"\n\nthing b:\n    v = \"b\"\n\n",
    );
    for i in 0..n {
        src.push_str(&format!("thing base{i} uses a, b:\n    v = \"x\"\n\n"));
    }
    src.push_str("thing top uses ");
    src.push_str(
        &(0..n)
            .map(|i| format!("base{i}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    src.push_str(":\n    v = \"t\"\n");
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    assert!(!out.status.success(), "over-cap stack must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2066"), "{stderr}");
    elapsed
}

/// Does doubling the input cost at most `factor` times the wall time?
///
/// The default-lane form of a complexity pin: a RATIO and nothing else,
/// because an absolute bound measures the machine's load and not the code.
/// No noise floor — a floor turns the pin back into the wall-clock bound it
/// exists to avoid whenever the small input lands under it, and then a
/// regression of exactly the shape being hunted fits underneath. Timer
/// granularity is answered by [`least_contended`]'s own assertion instead.
fn scale_holds(a: std::time::Duration, b: std::time::Duration, factor: f64) -> bool {
    b.as_secs_f64() <= factor * a.as_secs_f64()
}

/// The smallest input time a ratio may be taken over. The `nml` process
/// itself costs 2.8 ms here and the two pins' small inputs 62 ms and
/// 400 ms, so this is an assertion about the FIXTURE — if a pin's small
/// input ever becomes too cheap to divide by, it says so instead of
/// quietly measuring the clock.
const MIN_MEASURABLE: std::time::Duration = std::time::Duration::from_millis(10);

/// Sample `measure` at least twice, up to four times, keeping the SMALLEST
/// time seen for each size.
///
/// The minimum is the least-contended estimate: a stall inflates a sample
/// and can never deflate one. TWO pairs at minimum, and that is not an
/// optimisation — the first `nml` a test process spawns pays the binary's
/// cold start, which lands entirely on the SMALL input (measured: 424 ms
/// against 62 ms warm) and puts the ratio UNDER ONE, so a single-pair pin
/// passes whatever it is asked. The extra pairs beyond the second are for
/// scheduling noise: the whole workspace suite, run in parallel, has
/// stalled both samples of the larger input at once.
fn least_contended(
    measure: impl Fn() -> (std::time::Duration, std::time::Duration),
    factor: f64,
) -> (std::time::Duration, std::time::Duration) {
    let (cold_a, cold_b) = measure();
    let (warm_a, warm_b) = measure();
    let (mut a, mut b) = (cold_a.min(warm_a), cold_b.min(warm_b));
    for _ in 0..2 {
        if scale_holds(a, b, factor) {
            break;
        }
        let (a2, b2) = measure();
        a = a.min(a2);
        b = b.min(b2);
    }
    assert!(
        a >= MIN_MEASURABLE,
        "the small input took {a:?}, under {MIN_MEASURABLE:?} — too cheap to take a \
         ratio over: grow the fixture rather than reading timer noise as a verdict"
    );
    (a, b)
}

/// Security: the 16-layer cap must reject BEFORE the C3 merge — a
/// multi-thousand-ref clause used to buy minutes of cubic CPU from
/// kilobytes of input before the post-merge depth check saw it. The
/// default-lane pin is a scale RATIO ([`scale_holds`]), never a
/// wall-clock bound. The absolute bound lives in the release perf tier
/// (`perf_wide_uses_clause_is_rejected_quickly`).
///
/// The factor is MEASURED, not chosen. The work is linear, so the ratio is
/// ~2.0: 1.70-2.06 idle and 1.68-2.43 at load 15 on 16 cores, 18 samples.
/// A cubic term costs eight (the fixed process cost is 2.8 ms against a
/// 62 ms small input, so it barely discounts it), and in the shape this pin
/// exists for it cost minutes. Five sits 1.56x above the worst ratio ever
/// seen here and 1.56x below the signature it hunts. Three did not: the
/// whole workspace suite in parallel produced 102.86 ms for 750 refs and
/// 330.40 ms for 1500 — ratio 3.21, a RED gate about nothing. Sustained
/// contention inflates the LARGER run more than the smaller (a bigger
/// working set competing for memory bandwidth), so it is the ratio itself
/// that moves, and more samples do not bring it back.
#[test]
fn wide_uses_clause_is_rejected_near_linearly() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (small, large) = (750, 1500);
    let measure = || {
        (
            time_wide_clause_check(&dir, small),
            time_wide_clause_check(&dir, large),
        )
    };
    let (a, b) = least_contended(measure, 5.0);
    assert!(
        scale_holds(a, b, 5.0),
        "rejection is pre-merge and near-linear: {small} refs took {a:?}, {large} took {b:?}"
    );
}

/// The absolute bound the ratio pin above no longer carries: a
/// 1,500-ref clause is rejected within 5 s on the release,
/// single-threaded perf tier.
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_wide_uses_clause_is_rejected_quickly() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let elapsed = time_wide_clause_check(&dir, 1500);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "rejection is pre-merge and near-linear, took {elapsed:?}"
    );
}

#[test]
fn validate_flags_uses_on_schema_definitions() {
    // NML2062's schema-definition form is definition-intrinsic, so
    // `validate` owns it with `check`'s exact wording.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("schema-def-uses.nml");
    std::fs::write(&f, "model m uses other:\n    x string\n").unwrap();
    for verb in ["validate", "check"] {
        let out = nml_bin()
            .args([verb, f.to_str().unwrap()])
            .output()
            .expect("failed to run nml");
        assert!(!out.status.success(), "{verb} must reject the clause");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("NML2062"), "{verb}: {stderr}");
        assert!(stderr.contains("delete the clause"), "{verb}: {stderr}");
    }
}

#[test]
fn fix_applies_the_sealed_equal_value_deletion() {
    // The fixer composes (RFC 0019): the equal-value NML2060's deletion
    // suggestion is advertised as `nml fix`-eligible, and without
    // composing the diagnostic never exists in the fixer's world.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-sealed-restatement.nml");
    let src = "model flow:\n    entrypoint string #sealed\n\nflow base:\n    entrypoint = \"search\"\n\nflow t uses base:\n    entrypoint = \"search\"\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        !fixed.contains("flow t uses base:\n    entrypoint"),
        "the restated assignment is deleted\nstdout: {stdout}\nstderr: {stderr}\nfile:\n{fixed}"
    );
    assert!(
        !fixed.lines().any(|l| !l.is_empty() && l.trim().is_empty()),
        "a deletion takes its whole line — no indentation-only line is left behind:\n{fixed:?}"
    );
    // And the fixed file now checks clean.
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "post-fix file is clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The four `wide_*` wall-clock gates below (10 s bounds) flake under
/// concurrent build load (both r68 lanes and the r69a fold saw it), so
/// they run in the `perf_` tier — `#[ignore]`d by default, run on the
/// Linux CI lane with `--ignored perf_ --test-threads=1` (r69a C2, r69b
/// item 9) — keeping their bounds. The 5 s `wide_uses_clause_is_
/// rejected_quickly` pin stays in the default lane (it has not flaked).
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_wide_model_compose_is_not_quadratic() {
    // Security: per-entry linear scans of wide models made compose
    // O(width²) across layers — tens of seconds from a sub-megabyte
    // hostile file. Field lookups are mapped now; a wide fully-populated
    // stack must compose fast.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wide-model.nml");
    let width = 250;
    let mut src = String::from("model wide:\n");
    for i in 0..width {
        src.push_str(&format!("    f{i} string\n"));
    }
    src.push_str("\nwide base:\n");
    for i in 0..width {
        src.push_str(&format!("    f{i} = \"b\"\n"));
    }
    let mut prev = "base".to_string();
    for l in 0..15 {
        src.push_str(&format!("\nwide l{l} uses {prev}:\n"));
        for i in 0..width {
            src.push_str(&format!("    f{i} = \"v{l}\"\n"));
        }
        prev = format!("l{l}");
    }
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "wide stack composes clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "compose is near-linear in width, took {elapsed:?}"
    );
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_wide_union_bodies_compose_fast() {
    // Security: every union position folds variant decisions and (on a
    // switch) normalizes the displaced group for the seal scan — a wide
    // stack of union fields with per-layer switches must stay
    // near-linear, or hostile sub-megabyte input buys seconds of CPU.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wide-union.nml");
    let width = 250;
    let mut src =
        String::from("model ua:\n    x string\n\nmodel ub:\n    y string\n\nmodel wideu:\n");
    for i in 0..width {
        src.push_str(&format!("    u{i} (ua | ub)\n"));
    }
    src.push_str("\nwideu base:\n");
    for i in 0..width {
        src.push_str(&format!("    u{i} as ua:\n        x = \"b\"\n"));
    }
    let mut prev = "base".to_string();
    for l in 0..15 {
        let (variant, field, value) = if l % 2 == 0 {
            ("ub", "y", "v")
        } else {
            ("ua", "x", "w")
        };
        src.push_str(&format!("\nwideu l{l} uses {prev}:\n"));
        for i in 0..width {
            src.push_str(&format!(
                "    u{i} as {variant}:\n        {field} = \"{value}{l}\"\n"
            ));
        }
        prev = format!("l{l}");
    }
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "alternating unsealed switches compose clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "union compose is near-linear in width, took {elapsed:?}"
    );
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_wide_union_rejected_switches_compose_fast() {
    // The seal-scan axis the unsealed wide-union pin cannot guard: every
    // switch normalizes the displaced group for judgment — with seals
    // ASSIGNED, so the scan actually runs, width × layers times.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wide-union-sealed.nml");
    let width = 250;
    let mut src = String::from(
        "model ua:\n    x string\n    s string #sealed\n\nmodel ub:\n    y string\n\nmodel widesu:\n",
    );
    for i in 0..width {
        src.push_str(&format!("    u{i} (ua | ub)\n"));
    }
    src.push_str("\nwidesu base:\n");
    for i in 0..width {
        src.push_str(&format!("    u{i} as ua:\n        s = \"locked\"\n"));
    }
    let mut prev = "base".to_string();
    for l in 0..15 {
        src.push_str(&format!("\nwidesu l{l} uses {prev}:\n"));
        for i in 0..width {
            src.push_str(&format!("    u{i} as ub:\n        y = \"v{l}\"\n"));
        }
        prev = format!("l{l}");
    }
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    // `--max-findings 0` lifts the default finding-PRINTING budget
    // (r73-cli claim 5: `out::MAX_SHOWN` = 512, a per-code fair share of
    // 448) — this pin counts every one of the 3,750 rejections on stderr,
    // which is exactly the volume the budget exists to withhold. The
    // counts and the exit are exact either way; only the print is
    // bounded. (r74 merge: found by the perf tier, which neither r73 lane
    // ran against the other's change.)
    let out = nml_bin()
        .args(["check", "--max-findings", "0", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    assert!(
        !out.status.success(),
        "every switch is seal-rejected — the check must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("error[NML2060]").count(),
        width * 15,
        "every switch is rejected by the BACKSTOP (not some other error)"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "seal-judged switches stay near-linear in width, took {elapsed:?}"
    );
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_wide_list_variant_rejected_switches_scale_linearly_in_items() {
    // The Items-establishment axis: N sealed list items displaced by M
    // rejected switches was N×M full scans with an O(hits²) dedup —
    // super-linear (~3× per doubling) from a sub-megabyte file. The
    // judgment is memoized per unchanged group and dedups by hash now.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wide-list-variant-sealed.nml");
    let items = 2000;
    let mut src = String::from(
        "model ua:\n    x string\n\nmodel ub:\n    kind string\n    secret string #sealed\n\n\
         model holder:\n    slot (ua | []ub)\n\nholder base:\n    slot:\n",
    );
    for i in 0..items {
        src.push_str(&format!("        - w{i}:\n            secret = \"s\"\n"));
    }
    let mut prev = "base".to_string();
    for l in 0..15 {
        src.push_str(&format!(
            "\nholder l{l} uses {prev}:\n    slot as ua:\n        x = \"v{l}\"\n"
        ));
        prev = format!("l{l}");
    }
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("error[NML2060]").count(),
        15,
        "every switch off the sealed list is rejected:\n{}",
        stderr.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "list-variant judgment is near-linear in items, took {elapsed:?}"
    );
}

#[test]
fn inherited_empty_array_at_a_union_position_composes_clean() {
    // A valid inherited `slot = []` at `(ua | []ub)` must stay a valid
    // empty list on every dependent — never an empty OBJECT of the
    // first model variant (a phantom "missing required field").
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("inherited-empty-union-list.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string\n\nmodel ub:\n    kind string\n\n\
         model holder:\n    slot (ua | []ub)\n    label string\n\n\
         holder base:\n    slot = []\n    label = \"b\"\n\n\
         holder t uses base:\n    label = \"l\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("NML2007"), "{stderr}");
}

#[test]
fn zero_item_entries_at_union_positions_warn_exactly_once_per_spelling() {
    // `= []`, an empty block, `|slot = []`, `|slot:` — each zero-item
    // spelling at a union position warns exactly once through `check`
    // (no normalization+merge double, no re-report from dependents).
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("zero-item-union-spellings.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string\n\nmodel ub:\n    kind string\n\n\
         model holder:\n    slot (ua | []ub)\n\n\
         holder base:\n    slot:\n        - w:\n            kind = \"k\"\n\n\
         holder t1 uses base:\n    slot = []\n\n\
         holder t2 uses t1:\n    slot:\n\n\
         holder t3 uses t2:\n    |slot = []\n\n\
         holder t4 uses t3:\n    |slot:\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("warning[NML2079]").count(), 4, "{stderr}");
    assert!(out.status.success(), "{stderr}");
}

#[test]
fn type_annotation_modifier_at_a_union_position_never_panics_or_launders() {
    // The end-to-end face of the routing fix: a debug-build `check`
    // must not panic, and the sealed base must survive the annotated
    // switch (NML2060), never be laundered by a last-wins fallthrough.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("typeann-union.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string #sealed\n\nmodel ub:\n    y string\n\n\
         model holder:\n    slot (ua | ub)\n\n\
         holder base:\n    slot as ua:\n        x = \"1\"\n\n\
         holder top uses base:\n    |slot (ua | ub)\n    slot as ub:\n        y = \"2\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(stderr.matches("error[NML2060]").count(), 1, "{stderr}");
}

#[test]
fn shared_only_union_blocks_compose_clean_on_dependents() {
    // `.shared`-only blocks are zero-item entries raw and normalized
    // alike: the dependent composes as `slot = []` and validates clean
    // (the raw base's own empty-object reading is the validator's).
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("shared-only-union.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string\n\nmodel ub:\n    name string+\n    note string?\n\n\
         model h:\n    slot (ua | []ub)\n\n\
         h base:\n    slot:\n        .note = \"n\"\n\n\
         h t uses base:\n    slot:\n        .note = \"m\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("warning[NML2079]").count(), 2, "{stderr}");
    assert!(
        !stderr.contains("missing required field 'slot'"),
        "the field is never dropped: {stderr}"
    );
}

#[test]
fn sealed_union_bogus_as_reports_once_with_the_seal() {
    // A dependent's bogus `as` at a `#sealed` union position: NML2051
    // exactly once (the sealed route reports it too) beside the seal's
    // own NML2060.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("sealed-union-bogus-as.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string\n\nmodel ub:\n    y string\n\n\
         model holder:\n    slot (ua | ub) #sealed\n\n\
         holder base:\n    slot as ua:\n        x = \"1\"\n\n\
         holder top uses base:\n    slot as zz:\n        y = \"2\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert_eq!(stderr.matches("error[NML2051]").count(), 1, "{stderr}");
    assert_eq!(stderr.matches("error[NML2060]").count(), 1, "{stderr}");
}

#[test]
fn ambiguous_stack_reports_nml2052_once() {
    // An ambiguous base composed by dependents is ONE finding through
    // `check`: compose never guesses (the composed body stays
    // un-annotated) and the composed entry carries the establishing
    // span, so the raw and composed findings collapse to one home.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("ambiguous-stack.nml");
    std::fs::write(
        &f,
        "model stepA:\n    note string\n\nmodel stepB:\n    note string\n\n\
         model holder:\n    slot (stepA | stepB)\n\n\
         holder base:\n    slot:\n        note = \"1\"\n\n\
         holder t uses base:\n    slot:\n        note = \"2\"\n\n\
         holder t2 uses t:\n    slot:\n        note = \"3\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("error[NML2052]").count(), 1, "{stderr}");
    assert!(
        stderr.contains("add an explicit type with `as <variant>`"),
        "D2's teaching survives composition: {stderr}"
    );
}

#[test]
fn base_bogus_as_is_reported_exactly_once_with_dependents() {
    // The merge reports a swallowed NML2051 itself; a non-`uses` base's
    // raw validation re-derives the same finding — `check` must seed its
    // dedup with the composed diagnostics (LSP and `fix` already did),
    // or the pair prints twice.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("bogus-as-base.nml");
    std::fs::write(
        &f,
        "model card:\n    last4 string\n\nmodel cash:\n    amount string\n\n\
         model account:\n    payment (card | cash)\n\n\
         account base:\n    payment as cardd:\n        last4 = \"4242\"\n\n\
         account t uses base:\n    payment:\n        last4 = \"9999\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let n = stderr.matches("error[NML2051]").count();
    assert_eq!(n, 1, "one defect, one finding:\n{stderr}");
}

#[test]
fn explain_serves_the_union_codes() {
    for (code, needle) in [
        ("NML2085", "Discarded union contribution"),
        ("NML2086", "Internal composition invariant"),
        // Inline code in the index is content, never a link to strip:
        // the type spelling must render verbatim.
        ("NML2076", "such as `(a | []b)`"),
    ] {
        let out = nml_bin()
            .args(["explain", code])
            .output()
            .expect("failed to run nml");
        assert!(out.status.success(), "{code}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(needle), "{code}: {stdout}");
    }
}

/// An identity list of `n` items in a base flow and `n` overriding
/// items in a layer over it — the shape whose per-item lookups were
/// O(items²) — written to `dir` and checked; the wall time of the run.
fn time_identity_list_check(dir: &Path, n: usize) -> std::time::Duration {
    let f = dir.join(format!("large-list-{n}.nml"));
    let mut src = String::from(
        "model item:\n    name string+\n    v string\n\nmodel flow:\n    items []item #identity\n\nflow base:\n    items:\n",
    );
    for i in 0..n {
        src.push_str(&format!("        - n{i}:\n            v = \"x\"\n"));
    }
    src.push_str("\nflow t uses base:\n    items:\n");
    for i in 0..n {
        src.push_str(&format!("        - n{i}:\n            v = \"y\"\n"));
    }
    std::fs::write(&f, src).unwrap();
    let start = std::time::Instant::now();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "large identity stack composes clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    elapsed
}

/// Security: per-item linear scans over the resolved list and the
/// sibling item pool were O(items²) — seconds of CPU from sub-megabyte
/// hostile lists. Both lookups are bucketed now. The default-lane pin is
/// LOAD-INDEPENDENT: a scale RATIO ([`scale_holds`]) — doubling the items
/// costs less than three times the wall time (a quadratic term costs four)
/// — never a wall-clock bound, which failed under a concurrent build
/// (round 84). Three is the midpoint between linear and quadratic and
/// cannot be raised without blinding the pin, so this one leans on
/// [`least_contended`]'s four samples instead. Its margins are the
/// narrowest in the suite — measured 1.90-2.01 idle, so 1.5x of headroom
/// against noise and 1.33x against the quadratic term it hunts. The
/// absolute bound lives in the release perf tier
/// (`perf_large_identity_lists_compose_fast`).
#[test]
fn large_identity_lists_compose_near_linearly() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (small, large) = (4_000, 8_000);
    let measure = || {
        (
            time_identity_list_check(&dir, small),
            time_identity_list_check(&dir, large),
        )
    };
    let (a, b) = least_contended(measure, 3.0);
    assert!(
        scale_holds(a, b, 3.0),
        "item merge and seal scan are near-linear: {small} items took {a:?}, {large} took {b:?}"
    );
}

/// The absolute bound the ratio pin above no longer carries: 8,000
/// identity items compose within 5 s on the release, single-threaded
/// perf tier (the pre-bucketing scans took tens of seconds).
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_large_identity_lists_compose_fast() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let elapsed = time_identity_list_check(&dir, 8_000);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "item merge and seal scan are near-linear, took {elapsed:?}"
    );
}

#[test]
fn base_defect_reports_once_across_overlays() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("dedup.nml");
    std::fs::write(
        &f,
        "model m:\n    label string\n\nm base:\n    label = \"x\"\n    typo = 1\n\nm o1 uses base:\n    label = \"y\"\n\nm o2 uses base:\n    label = \"z\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let count = stderr.matches("unknown property 'typo'").count();
    assert_eq!(count, 1, "one home per finding: {stderr}");
}

#[test]
fn failed_compose_does_not_cascade_schema_errors() {
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("cycle-cascade.nml");
    std::fs::write(
        &f,
        "model m:\n    region string\n    label string\n\nm a uses b:\n    label = \"a\"\n\nm b uses a:\n    label = \"b\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("NML2061"), "{stderr}");
    assert!(
        !stderr.contains("NML2007"),
        "no missing-required cascade on engine refusal: {stderr}"
    );
}

#[test]
fn a_set_variant_ahead_of_the_list_variant_cannot_launder_sealed_items() {
    // Round-17 regression: `(ua | set<string> | []ub)` judged the displaced
    // list under `string` (no vocabulary, no scan) and the switch composed
    // `ok` with the sealed item body discarded silently. Block items resolve
    // to the first `List` everywhere; the backstop binds there.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("set-first-union-seal.nml");
    std::fs::write(
        &f,
        "model ua:\n    x string\n\nmodel ub:\n    kind string\n    secret string #sealed\n\n\
         model holder:\n    slot (ua | set<string> | []ub)\n\n\
         holder base:\n    slot:\n        - w:\n            kind = \"k\"\n            secret = \"s\"\n\n\
         holder top uses base:\n    slot as ua:\n        x = \"1\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2060]").count(), 1, "{stderr}");
    assert!(stderr.contains("slot[w].secret"), "{stderr}");
}

#[test]
fn a_non_item_line_in_a_modifier_block_is_loud_at_its_own_position() {
    // Named by its kind, anchored on the line (column 9, not the indent),
    // never "found end of file" mid-file; `fmt` refuses rather than
    // dropping it.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("modifier-block-shared-line.nml");
    std::fs::write(
        &f,
        "model policy:\n    |deny []string\n\n\
         policy p:\n    |deny:\n        - \"a\"\n        .note = \"x\"\n        - \"b\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            ":7:9: error[NML0002]: expected a list item in a modifier block, found a shared property"
        ),
        "{stderr}"
    );
    let before = std::fs::read_to_string(&f).unwrap();
    let out = nml_bin()
        .args(["fmt", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        !out.status.success(),
        "fmt refuses a file it cannot lower losslessly"
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        before,
        "and leaves it untouched"
    );
}

#[test]
fn an_empty_array_on_a_declared_scalar_modifier_is_a_type_error_on_the_composed_view() {
    // `|label = []` above `|label string` reaches the composed view as a
    // value (never a zero-item no-op) — and the validator says so.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("scalar-modifier-empty-array.nml");
    std::fs::write(
        &f,
        "model m2:\n    |label string\n\nm2 base:\n    |label = \"a\"\n\nm2 t uses base:\n    |label = []\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2008]").count(), 1, "{stderr}");
    assert!(stderr.contains("expected string, got array"), "{stderr}");
}

#[test]
fn a_dependents_non_string_discriminator_is_nml2042() {
    // Composition re-adds the effective string discriminator ahead of the
    // dependent's `kind = 5`; a first-only validator check laundered it.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("dependent-non-string-discriminator.nml");
    std::fs::write(
        &f,
        "model arma:\n    kind string\n    a string\n\nmodel armb:\n    kind string\n    b string\n\n\
         oneof oo by kind:\n    \"a\" -> arma\n    \"b\" -> armb\n\nmodel h:\n    cfg oo\n\n\
         h base:\n    cfg:\n        kind = \"b\"\n        b = \"1\"\n\n\
         h top uses base:\n    cfg:\n        kind = 5\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2042]").count(), 1, "{stderr}");
}

#[test]
fn both_invalid_discriminators_are_each_reported() {
    // Part C (RFC 0019 E16): base `kind = 5`, dependent `kind = 6`.
    // Stripping by NAME passes both through (`kind = 5, kind = 6` at
    // the front of the composed view) and the every-entry check reports
    // each at its author's span; the base's collapses onto its raw
    // home. Before E16, `kind = 6` silently overlaid `kind = 5`.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("both-invalid-discriminators.nml");
    let src = concat!(
        "model arma:\n    a string\n\nmodel armb:\n    b string\n\n",
        "oneof oo by kind:\n    \"a\" -> arma\n    \"b\" -> armb\n\n",
        "model h:\n    cfg oo\n\n",
        "h base:\n    cfg:\n        kind = 5\n\n",
        "h top uses base:\n    cfg:\n        kind = 6\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2042]").count(), 2, "{stderr}");
    let line_of = |needle: &str| src[..src.find(needle).unwrap()].lines().count();
    let base_kind = line_of("kind = 5");
    let top_kind = line_of("kind = 6");
    assert!(
        stderr.contains(&format!(":{base_kind}:")),
        "base's span: {stderr}"
    );
    assert!(
        stderr.contains(&format!(":{top_kind}:")),
        "top's span: {stderr}"
    );
}

/// NML2054 is an error at schema load, at the FIELD (its content, not its
/// indentation) with the union's declaration as a note; the row carries
/// the field's deletion, which `fix` applies — the comment above the
/// field stays and the file then loads clean. `--json` carries the
/// deletion resolved to the field's row.
#[test]
fn a_shadowed_discriminator_is_an_error_at_the_field_and_fix_deletes_it() {
    let dir = scratch_dir("shadow-own");
    let root = dir.to_str().unwrap();
    let f = dir.join("shadow.model.nml");
    let src = "model logEntry:\n    // the entry's kind\n    kind string?\n    msg string?\n\n\
               oneof record by kind:\n    \"log\" -> logEntry\n";
    std::fs::write(&f, src).unwrap();
    let file = f.to_str().unwrap();
    let out = nml_bin()
        .args(["check", "--root", root, file])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "shadow.model.nml:3:5: error[NML2054]: oneof 'record' arm \"log\": model 'logEntry' declares a field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so the field can never be set; delete it (to forbid arm switching instead, seal it: `kind string? #sealed`)"
        ),
        "{stderr}"
    );
    assert!(
        stderr
            .contains("shadow.model.nml:6:1: note: oneof 'record' selects its arm by 'kind' here"),
        "{stderr}"
    );
    assert!(stderr.contains("error: 1 error(s)"), "{stderr}");
    let out = nml_bin()
        .args(["check", "--json", "--root", root, file])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["code"] == "NML2054")
        .unwrap_or_else(|| panic!("no NML2054 row in {stdout}"));
    assert_eq!(row["severity"], "error", "{row}");
    assert_eq!(
        (row["line"].as_u64(), row["col"].as_u64()),
        (Some(3), Some(5)),
        "{row}"
    );
    assert_eq!(
        row["suggestions"],
        serde_json::json!([{
            "kind": "delete",
            "source": "shadow.model.nml",
            "edits": [{ "line": 3, "col": 1, "endLine": 4, "endCol": 1, "lines": [""] }]
        }]),
        "the field's row: {row}"
    );
    assert_eq!(
        row["related"],
        serde_json::json!([{
            "line": 6, "col": 1, "source": "shadow.model.nml",
            "message": "oneof 'record' selects its arm by 'kind' here"
        }]),
        "{row}"
    );
    let out = nml_bin()
        .args(["fix", "--root", root, file])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{all}");
    assert!(
        all.contains("1 edit(s) applied across 1 of 1 file(s); 0 diagnostic(s) not auto-fixable"),
        "{all}"
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "model logEntry:\n    // the entry's kind\n    msg string?\n\noneof record by kind:\n    \
         \"log\" -> logEntry\n",
        "the row alone is gone; the comment stays"
    );
    let out = nml_bin()
        .args(["check", "--root", root, file])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A required `#sealed` field of the discriminator's name is the shape
/// wearing the seal's directive: refused at the field, the `?` that makes
/// it the sanctioned optional spelling as the one fix — a zero-width
/// insertion at the type's end, which `fix` applies.
#[test]
fn a_required_seal_is_refused_and_fixed_to_the_optional_spelling() {
    let dir = scratch_dir("shadow-seal");
    let root = dir.to_str().unwrap();
    let f = dir.join("sealed.model.nml");
    let src = "model logEntry:\n    kind string #sealed\n    msg string?\n\noneof record by kind:\n    \
               \"log\" -> logEntry\n";
    std::fs::write(&f, src).unwrap();
    let file = f.to_str().unwrap();
    let out = nml_bin()
        .args(["check", "--root", root, file])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("sealed.model.nml:2:5: error[NML2054]: oneof 'record' arm \"log\": model 'logEntry' seals the discriminator with a required field 'kind' — an instance's 'kind' property is always read as the discriminator, so a required field can never be satisfied; declare it optional: `kind string? #sealed` (fix: `?`)"),
        "{stderr}"
    );
    let out = nml_bin()
        .args(["check", "--json", "--root", root, file])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["code"] == "NML2054")
        .unwrap_or_else(|| panic!("no NML2054 row in {stdout}"));
    assert_eq!(
        row["suggestions"],
        serde_json::json!([{
            "kind": "fix",
            "source": "sealed.model.nml",
            "edits": [{ "line": 2, "col": 16, "endLine": 2, "endCol": 16, "lines": ["?"] }]
        }]),
        "the `?` at the type's end: {row}"
    );
    let out = nml_bin()
        .args(["fix", "--root", root, file])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "model logEntry:\n    kind string? #sealed\n    msg string?\n\noneof record by kind:\n    \
         \"log\" -> logEntry\n"
    );
    let out = nml_bin()
        .args(["check", "--root", root, file])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A field reaching the arm through `is`: the row at the arm's `is`
/// reference, the declaring definition named, the field and the union as
/// notes, and NO fix — `fix` leaves the file untouched and counts the row
/// as not auto-fixable.
#[test]
fn an_inherited_shadow_is_located_at_the_mixin_with_no_fix() {
    let dir = scratch_dir("shadow-mixin");
    let root = dir.to_str().unwrap();
    let f = dir.join("mixed.model.nml");
    let src = "trait tagged:\n    kind string?\n\nmodel logEntry is tagged:\n    msg string?\n\n\
               model note is tagged:\n    text string?\n\noneof record by kind:\n    \
               \"log\" -> logEntry\n";
    std::fs::write(&f, src).unwrap();
    let file = f.to_str().unwrap();
    let out = nml_bin()
        .args(["check", "--root", root, file])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("mixed.model.nml:4:19: error[NML2054]: oneof 'record' arm \"log\": model 'logEntry' inherits a field 'kind' named like the discriminator from trait 'tagged' — an instance's 'kind' property is always read as the discriminator, so the field can never be set in this arm; rename the discriminator, drop `is tagged`, or delete the field where it is declared"), "{stderr}");
    assert!(
        stderr.contains("mixed.model.nml:2:5: note: field 'kind' declared here"),
        "{stderr}"
    );
    assert!(
        stderr
            .contains("mixed.model.nml:10:1: note: oneof 'record' selects its arm by 'kind' here"),
        "{stderr}"
    );
    let out = nml_bin()
        .args(["fix", "--root", root, file])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("0 edit(s) applied across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable"),
        "{all}"
    );
    assert_eq!(std::fs::read_to_string(&f).unwrap(), src, "untouched");
}

/// Under a `--schema` directory the load's errors are reported and the
/// instance is still judged (the directory posture for every load error):
/// the schema's NML2054 row, then the instance's own missing-field row
/// the required shape forces — the row that made every instance fail
/// while the schema merely warned.
#[test]
fn an_instance_under_a_shadowed_schema_dir_sees_the_schema_row_and_its_own() {
    let dir = scratch_dir("shadow-schema-dir");
    let root = dir.to_str().unwrap();
    std::fs::write(
        dir.join("s.model.nml"),
        "model logEntry:\n    kind string\n    msg string?\n\noneof record by kind:\n    \
         \"log\" -> logEntry\n\nmodel host:\n    rec record\n",
    )
    .unwrap();
    let i = dir.join("i.nml");
    std::fs::write(&i, "host H:\n    rec:\n        kind = \"log\"\n").unwrap();
    let out = nml_bin()
        .args([
            "check",
            "--root",
            root,
            "--schema",
            root,
            i.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let schema_row = stderr
        .find("s.model.nml:2:5: error[NML2054]")
        .unwrap_or_else(|| panic!("{stderr}"));
    let instance_row = stderr
        .find("i.nml:2:5: error[NML2007]: missing required field 'kind' (defined in model 'logEntry')")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(
        schema_row < instance_row,
        "the schema's row first: {stderr}"
    );
    assert!(stderr.contains("error: 2 error(s)"), "{stderr}");
}

/// A file governed by a binding whose declared source carries the shape:
/// NML2091, its `related` row the NML2054 finding at the field in the
/// source's own file — the instance side's view of the cause — and the
/// source's own remedy riding the row in the source's file (RFC 0026
/// B-24: `suggestions[0].source` names the source, the edit resolved
/// against ITS text), so a consumer of the governed file's row applies
/// the fix where it lands; `nml fix` on the governed file rewrites
/// nothing (the row is the universe's, not a target finding).
#[test]
fn a_bound_file_under_a_shadowed_source_is_nml2091_with_the_cause_as_its_note() {
    let ws = fixture("shadowed-discriminator/bound");
    let out = nml_bin()
        .current_dir(&ws)
        .args([
            "check",
            "--json",
            "--root",
            ".",
            "tenants/cu/plain.flow.nml",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["code"] == "NML2091")
        .unwrap_or_else(|| panic!("no NML2091 row in {stdout}"));
    assert_eq!(row["source"], "tenants/cu/plain.flow.nml", "{row}");
    assert_eq!(
        row["related"],
        serde_json::json!([{
            "line": 3, "col": 5, "source": "core.model.nml",
            "message": "oneof 'record' arm \"log\": model 'logEntry' declares a field 'kind' named like the discriminator — an instance's 'kind' property is always read as the discriminator, so the field can never be set; delete it (to forbid arm switching instead, seal it: `kind string? #sealed`)"
        }]),
        "{row}"
    );
    // The RESOLVED edit, against the SOURCE's text: the CST kernel
    // expands a `Delete` over a property to the whole property line
    // (line 3 of core.model.nml, `    kind string?`), so applying it
    // leaves no blank line behind.
    assert_eq!(
        row["suggestions"],
        serde_json::json!([{
            "kind": "delete",
            "source": "core.model.nml",
            "edits": [{ "line": 3, "col": 1, "endLine": 4, "endCol": 1, "lines": [""] }],
        }]),
        "the source's remedy, in the source's file: {row}"
    );
    assert_eq!(row["cause"]["code"], "NML2054", "{row}");
    assert_eq!(row["cause"]["source"], "core.model.nml", "{row}");
    // The human surface says the same thing in the same file: the row on
    // the governed file, the note at the cause in the SOURCE's file. The
    // source's LOGICAL name (`core`) is prose in the message, never a
    // file: no line opens with it, and no remedy names it (a `Delete`
    // renders inline — only an `Insert` gets a `help:` block).
    let out = nml_bin()
        .current_dir(&ws)
        .args(["check", "--root", ".", "tenants/cu/plain.flow.nml"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tenants/cu/plain.flow.nml: error[NML2091]: binding 'tenantFlows' of demo.package.nml cannot build its validator: declared source `core` failed to load at core.model.nml:3:5:"),
        "{stderr}"
    );
    assert!(
        stderr.contains("\ncore.model.nml:3:5: note: oneof 'record' arm \"log\""),
        "{stderr}"
    );
    assert!(
        !stderr
            .lines()
            .any(|l| l.starts_with("core:") || l.starts_with("core.model.nml:3:5: help")),
        "no surface spells the logical name as a file, and a delete gets no block: {stderr}"
    );
    let before = std::fs::read_to_string(ws.join("core.model.nml")).unwrap();
    let out = nml_bin()
        .current_dir(&ws)
        .args(["fix", "--root", ".", "tenants/cu/plain.flow.nml"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    // The fourth surface of a wrapped remedy (the CHANGELOG's "the same
    // way"): the run rewrites nothing and its closing line says where
    // the edit is pending — the accounting a failed manifest's row gets
    // at the door.
    // The tally is the verb's RESULT line (stdout); the rows are stderr.
    let tally = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        tally.contains("1 edit(s) belong to another file — pending there"),
        "{tally}"
    );
    let out = nml_bin()
        .current_dir(&ws)
        .args(["fix", "--root", ".", "--json", "tenants/cu/plain.flow.nml"])
        .output()
        .unwrap();
    let summary = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "summary")
        .expect("summary row");
    assert_eq!(summary["routed"], 1, "{summary}");
    assert_eq!(summary["edits"], 0, "{summary}");
    assert_eq!(
        std::fs::read_to_string(ws.join("core.model.nml")).unwrap(),
        before,
        "the governed file's run never edits the source"
    );
}

#[test]
fn the_nml2054_shape_is_a_repeated_name_before_any_compose_verdict() {
    // An arm model declares a union FIELD named like the discriminator
    // (the NML2054 shape). An instance of it must state the
    // discriminator (`kind = "a"`) beside the field's body (`kind as
    // va2:`) — one body, one name, two spellings — which the parse
    // refuses (NML2093) before any compose verdict is drawn: `check`
    // prints the repeat and stops, as it stops at every parse finding.
    // The NML2042-not-NML2085 verdict over the composed tree stays the
    // kernel battery's (`a_model_typed_discriminator_named_field_keeps_
    // block_and_string_apart`), where the editor's best-effort path
    // reaches it.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("nml2054-shape-verdict.nml");
    let src = concat!(
        "model va2:\n    x string\n\nmodel vb2:\n    y string\n\n",
        "model arm:\n    kind (va2 | vb2)\n\n",
        "oneof oo by kind:\n    \"a\" -> arm\n\n",
        "model h:\n    cfg oo\n\n",
        "h base:\n    cfg:\n        kind = \"a\"\n        kind as va2:\n            x = \"1\"\n\n",
        "h top uses base:\n    cfg:\n        kind = 5\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let block = src[..src.find("kind as va2").unwrap()].lines().count();
    assert!(
        stderr.contains(&format!(
            ":{block}:9: error[NML2093]: duplicate entry 'kind' — a body declares each name once \
             (`kind:` and `kind = …` are two spellings of one entry)"
        )),
        "the base's later `kind`: {stderr}"
    );
    assert!(stderr.contains("error: 1 parse error(s)"), "{stderr}");
    assert!(
        !stderr.contains("NML2042"),
        "no verdict on an ill-formed body: {stderr}"
    );
    assert!(
        !stderr.contains("NML2085"),
        "no verdict on an ill-formed body: {stderr}"
    );
}

#[test]
fn a_non_string_restatement_draws_two_nml2042_and_no_dead_delta() {
    // `kind = 5` over `kind = 5`: both pass through — nothing overlays,
    // so the NML2084 dead-delta cannot fire — and the every-entry check
    // reports each at its author's span.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("non-string-restatement.nml");
    let src = concat!(
        "model arma:\n    a string\n\n",
        "oneof oo by kind:\n    \"a\" -> arma\n\n",
        "model h:\n    cfg oo\n\n",
        "h base:\n    cfg:\n        kind = 5\n\n",
        "h top uses base:\n    cfg:\n        kind = 5\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2042]").count(), 2, "{stderr}");
    assert!(!stderr.contains("NML2084"), "no dead delta: {stderr}");
}

#[test]
fn fix_deletions_remove_the_items_whole_row() {
    // The item-level sealed restatement: the deleted assignment's line
    // vanishes entirely (indentation and line break), the sibling stays.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-sealed-item-restatement.nml");
    std::fs::write(
        &f,
        "model step:\n    name string+\n    action string #sealed\n    note string?\n\n\
         model flow:\n    steps []step #identity\n\n\
         flow base:\n    steps:\n        - a:\n            action = \"x\"\n\n\
         flow t uses base:\n    steps:\n        - a:\n            action = \"x\"\n            note = \"n\"\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.ends_with("flow t uses base:\n    steps:\n        - a:\n            note = \"n\"\n"),
        "the restated line is gone, the sibling stays: {stderr}\n{fixed:?}"
    );
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_deletions_keep_crlf_line_endings_and_trailing_comments() {
    // The resolver's row walks take the CRLF terminator with the row
    // (the file stays CRLF; the `\r` is a Whitespace token before the
    // Newline), and a deletion with a trailing comment leaves the
    // comment at the line's indentation — a file `fmt` accepts as is.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-crlf-and-comment.nml");
    std::fs::write(
        &f,
        "model flow:\r\n    entrypoint string #sealed\r\n    note string\r\n\r\n\
         flow base:\r\n    entrypoint = \"search\"\r\n    note = \"n\"\r\n\r\n\
         flow t uses base:\r\n    entrypoint = \"search\"  // keep me\r\n    note = \"m\"\r\n",
    )
    .unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.ends_with("flow t uses base:\r\n    // keep me\r\n    note = \"m\"\r\n"),
        "{stderr}\n{fixed:?}"
    );
    assert!(
        !fixed.contains("\n\n\r") && fixed.matches("\r\n").count() == fixed.matches('\n').count(),
        "still CRLF: {fixed:?}"
    );
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_backstop_rejection_points_at_every_discarded_assignment() {
    // Two items each carrying a sealed field: the message counts them and
    // one `sealed here` note per assignment follows.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("backstop-notes.nml");
    std::fs::write(
        &f,
        concat!(
            "model ua:\n    x string\n\nmodel ub:\n    kind string\n    secret string #sealed\n\n",
            "model holder:\n    slot (ua | []ub)\n\n",
            "holder base:\n    slot:\n        - w:\n            kind = \"k\"\n            secret = \"s\"\n",
            "        - v:\n            kind = \"k\"\n            secret = \"t\"\n\n",
            "holder top uses base:\n    slot as ua:\n        x = \"1\"\n",
        ),
    )
    .unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("error[NML2060]").count(), 1, "{stderr}");
    assert!(
        stderr.contains("'slot[w].secret' (and 1 more field)"),
        "{stderr}"
    );
    assert_eq!(stderr.matches("note: sealed here").count(), 2, "{stderr}");
}

#[test]
fn two_switching_dependents_report_their_own_missing_fields() {
    // The finding-loss regression (RFC 0019 E15). Two dependents both
    // switch a oneof-typed field away from the base; each composed body
    // is missing the new arm's required field. Anchored at the BASE's
    // entry the two findings were one (code, span, message) key and
    // collapsed to one; the head rule anchors each at its own switching
    // layer.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("head-rule-two-dependents.nml");
    let src = concat!(
        "model va:\n    a string\n\n",
        "model vb:\n    b string\n\n",
        "oneof oo by kind = \"va\":\n    \"va\" -> va\n    \"vb\" -> vb\n\n",
        "model holder:\n    cfg oo\n\n",
        "holder base:\n    cfg:\n        kind = \"va\"\n        a = \"x\"\n\n",
        "holder dep1 uses base:\n    cfg:\n        kind = \"vb\"\n\n",
        "holder dep2 uses base:\n    cfg:\n        kind = \"vb\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("error[NML2007]").count(),
        2,
        "one finding per switching dependent: {stderr}"
    );
    // Each anchors at its OWN dependent's `cfg:` line.
    let cfg_line_after = |block: &str| {
        let at = src.find(block).unwrap();
        let cfg = at + src[at..].find("cfg:").unwrap();
        src[..cfg].lines().count()
    };
    let l1 = cfg_line_after("holder dep1");
    let l2 = cfg_line_after("holder dep2");
    assert!(
        stderr.contains(&format!(":{l1}:")),
        "dep1's anchor: {stderr}"
    );
    assert!(
        stderr.contains(&format!(":{l2}:")),
        "dep2's anchor: {stderr}"
    );
}

#[test]
fn two_switching_dependents_report_item_scope_findings_separately() {
    // The same finding-loss regression at ITEM scope: two dependents
    // each switch an identity item's arm; each merged item carries its
    // own switching span, so the two missing-field findings keep two
    // homes.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("head-rule-two-dependents-items.nml");
    let src = concat!(
        "model va:\n    name string+\n    a string\n\n",
        "model vb:\n    name string+\n    b string\n\n",
        "oneof oo by kind = \"va\":\n    \"va\" -> va\n    \"vb\" -> vb\n\n",
        "model holder:\n    xs []oo #identity\n\n",
        "holder base:\n    xs:\n        - w:\n            kind = \"va\"\n            a = \"x\"\n\n",
        "holder dep1 uses base:\n    xs:\n        - w:\n            kind = \"vb\"\n\n",
        "holder dep2 uses base:\n    xs:\n        - w:\n            kind = \"vb\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("error[NML2007]").count(),
        2,
        "one finding per switching dependent: {stderr}"
    );
    let item_line_after = |block: &str| {
        let at = src.find(block).unwrap();
        let item = at + src[at..].find("- w:").unwrap();
        src[..item].lines().count()
    };
    let l1 = item_line_after("holder dep1");
    let l2 = item_line_after("holder dep2");
    assert!(
        stderr.contains(&format!(":{l1}:")),
        "dep1's anchor: {stderr}"
    );
    assert!(
        stderr.contains(&format!(":{l2}:")),
        "dep2's anchor: {stderr}"
    );
}

#[test]
fn a_switch_chain_reports_once_at_the_switching_layer() {
    // base → mid switches → top joins: mid's own composition and top's
    // both miss the same required field, and under the head rule both
    // anchor at MID's `cfg:` — the layer that produced the body — so
    // the one-home dedup collapses only true duplicates.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("head-rule-chain.nml");
    let src = concat!(
        "model va:\n    a string\n\n",
        "model vb:\n    b string\n\n",
        "oneof oo by kind = \"va\":\n    \"va\" -> va\n    \"vb\" -> vb\n\n",
        "model holder:\n    cfg oo\n\n",
        "holder base:\n    cfg:\n        kind = \"va\"\n        a = \"x\"\n\n",
        "holder mid uses base:\n    cfg:\n        kind = \"vb\"\n\n",
        "holder top uses mid:\n    cfg:\n        kind = \"vb\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("error[NML2007]").count(),
        1,
        "one home at the switching layer: {stderr}"
    );
    let at = src.find("holder mid").unwrap();
    let cfg = at + src[at..].find("cfg:").unwrap();
    let line = src[..cfg].lines().count();
    assert!(
        stderr.contains(&format!(":{line}:")),
        "anchored at mid's cfg: {stderr}"
    );
}

#[test]
fn fix_applies_a_reveal_chain_to_convergence() {
    // The false-fixpoint probe (RFC 0023 A.3): applying NML2077's ref
    // deletion REVEALS an NML2060 (composition was aborted before), so a
    // raw finding-count gate saw 1 → 1 and stalled. The multiset gate
    // keys on the applied diagnostics; the file converges.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-reveal-chain.nml");
    let src = concat!(
        "model spec:\n    x string #sealed\n    y string?\n\n",
        "spec base:\n    x = \"1\"\n\n",
        "spec mid uses base:\n    y = \"2\"\n\n",
        "spec top uses mid, base:\n    x = \"1\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert_eq!(
        fixed,
        concat!(
            "model spec:\n    x string #sealed\n    y string?\n\n",
            "spec base:\n    x = \"1\"\n\n",
            "spec mid uses base:\n    y = \"2\"\n\n",
            "spec top uses mid\n",
        ),
        "the ref deletion lands, then the revealed restatement, then the\n\
         emptied clause header loses its colon: {stderr}"
    );
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_retries_a_compound_reveal_as_the_first_applied_candidate() {
    // The compound-reveal probe: one round applies dep1's NML2060
    // deletion AND top's NML2077 ref deletion, but the 2077 repair
    // un-suppresses top's NML2060 with the IDENTICAL message (the
    // message names the field, not the block) — the batch fails the
    // decrement. The singleton retry lands the first applied candidate
    // alone and the file still converges.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-compound-reveal.nml");
    let src = concat!(
        "model spec:\n    x string #sealed\n    y string?\n\n",
        "spec base:\n    x = \"1\"\n\n",
        "spec dep1 uses base:\n    x = \"1\"\n\n",
        "spec mid uses base:\n    y = \"2\"\n\n",
        "spec top uses mid, base:\n    x = \"1\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert_eq!(
        fixed,
        concat!(
            "model spec:\n    x string #sealed\n    y string?\n\n",
            "spec base:\n    x = \"1\"\n\n",
            "spec dep1 uses base\n\n",
            "spec mid uses base:\n    y = \"2\"\n\n",
            "spec top uses mid\n",
        ),
        "converges across retried rounds: {stderr}"
    );
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_accepts_a_repair_revealing_more_instances_of_an_unapplied_key() {
    // The unequal-value NML2060 carries no fix, so its key is never
    // applied — and a repaired ref revealing MORE instances of it must
    // not revert the repair.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-unapplied-key-reveal.nml");
    let src = concat!(
        "model spec:\n    x string #sealed\n    y string?\n\n",
        "spec base:\n    x = \"1\"\n\n",
        "spec dep1 uses base:\n    x = \"2\"\n\n",
        "spec mid uses base:\n    y = \"2\"\n\n",
        "spec top uses mid, base:\n    x = \"3\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.contains("spec top uses mid:\n    x = \"3\"\n"),
        "the ref deletion landed: {fixed:?}"
    );
    assert!(
        stdout.contains("2 diagnostic(s) not auto-fixable"),
        "both unequal restatements remain, reported: {stdout}"
    );
}

#[test]
fn fix_skips_a_refused_candidate_when_retrying() {
    // A `.shared`-distributed restatement's span is the synthesized
    // property's and matches no node — refused (`NoNodeAt`), PRINTED,
    // and skipped by the singleton retry, which lands the first APPLIED
    // candidate instead.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-refused-candidate-retry.nml");
    let src = concat!(
        "model item:\n    name string+\n    secret string #sealed\n\n",
        "model spec:\n    xs []item #identity\n    x string #sealed\n    y string?\n\n",
        "spec base:\n    xs:\n        - w:\n            secret = \"s\"\n    x = \"1\"\n\n",
        "spec shared uses base:\n    xs:\n        .secret = \"s\"\n        - w:\n\n",
        "spec dep1 uses base:\n    x = \"1\"\n\n",
        "spec mid uses base:\n    y = \"2\"\n\n",
        "spec top uses mid, base:\n    x = \"1\"\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("fix refused: no deletable node at this span"),
        "the refusal is printed, never silent: {stderr}"
    );
    // The full line shape: `<file>:<line>:<col>: fix refused: <reason>`,
    // anchored at the refused suggestion's own line.
    let shared_at = src.find(".secret").unwrap();
    let shared_line = src[..shared_at].lines().count();
    // The refused span is the synthesized property's name token — the
    // identifier after the dot; 1-based column.
    let shared_col = shared_at - src[..shared_at].rfind('\n').map_or(0, |i| i + 1) + 2;
    let refusal = stderr
        .lines()
        .find(|l| l.contains("fix refused"))
        .expect("a refusal line");
    assert!(
        refusal.starts_with(&format!(
            "{}:{shared_line}:{shared_col}: fix refused:",
            f.display()
        )),
        "path, line and column anchor the refusal: {refusal}"
    );
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.contains("spec dep1 uses base\n") && fixed.contains("spec top uses mid\n"),
        "the applied candidates landed around the refused one: {fixed:?}"
    );
    assert!(
        fixed.contains(".secret = \"s\""),
        "the refused restatement stays: {fixed:?}"
    );
    assert!(
        stdout.contains("1 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
}

#[test]
fn fix_keeps_fmt_clean_fixtures_fmt_clean() {
    // RFC 0023 A.5 — the canonicality property: for every fmt-clean
    // fixture under tests/fixtures/** and docs/**, `nml fix` (with the
    // fixture's own directory as its schema) leaves it fmt-clean and a
    // second run applies zero edits — the fix analogue of compose
    // idempotence. Holds by construction (deletions are token-exact,
    // rewrites are same-token) and is the ratchet for the day a producer
    // targets an aligned construct.
    fn nml_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if p.is_dir() {
                nml_files(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("nml") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut fixtures = Vec::new();
    nml_files(&root.join("tests/fixtures"), &mut fixtures);
    nml_files(&root.join("docs"), &mut fixtures);
    // A fixture that exists to FAIL to load has no fixpoint: `fix`
    // refuses the whole universe before any file, by design.
    // `workspace-unloadable` backs the error index's executed NML2088
    // transcript (its manifest declares a source that is deliberately
    // absent); `workspace-grant-bad` backs NML2081's (its `layers:`
    // grant breaks a loader rule); `workspace-dup` backs NML2093's (its
    // manifest names `files` twice); every universe under
    // `manifest-rules` backs one loader rule's transcript (NML2094 to
    // NML2102, NML2104, and the did-you-mean that rides NML2088);
    // `directive-reserved` backs NML2082's (its manifest redeclares the
    // language's `sealed` directive); `directive-collide` backs NML2082's
    // too (its vocabulary redeclares the language's `sealed`).
    fixtures.retain(|f| {
        !f.components().any(|c| {
            c.as_os_str() == "workspace-unloadable"
                || c.as_os_str() == "workspace-grant-bad"
                || c.as_os_str() == "workspace-dup"
                || c.as_os_str() == "manifest-rules"
                || c.as_os_str() == "directive-reserved"
                || c.as_os_str() == "directive-collide"
        })
    });
    let mut checked = 0usize;
    for fixture in &fixtures {
        let src = std::fs::read_to_string(fixture).unwrap();
        let Ok(formatted) = nml_fmt::formatter::format_source(&src) else {
            continue;
        };
        if formatted != src {
            continue;
        }
        checked += 1;
        // A fresh copy of the fixture's directory: its siblings are its
        // schema.
        let tmp = std::env::temp_dir().join(format!(
            "nml-fix-canonical-{}-{checked}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        for e in std::fs::read_dir(fixture.parent().unwrap())
            .unwrap()
            .flatten()
        {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("nml") {
                std::fs::copy(&p, tmp.join(p.file_name().unwrap())).unwrap();
            }
        }
        let target = tmp.join(fixture.file_name().unwrap());
        // Which door: a directory that carries a package manifest is a
        // closed universe whose binding SUPPLIES the schema, and `--schema`
        // there is a usage error by contract ("a CI flag cannot substitute
        // another vocabulary inside a closed universe"). Pin the root
        // instead, so a manifest-governed fixture is exercised rather than
        // skipped — tutorial chapter 9 is the first of them.
        let governed = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".package.nml"));
        let door = if governed { "--root" } else { "--schema" };
        let run = |label: &str| -> String {
            let out = nml_bin()
                .args(["fix", door, tmp.to_str().unwrap(), target.to_str().unwrap()])
                .output()
                .unwrap_or_else(|e| panic!("{label} on {fixture:?}: {e}"));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        run("first fix");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            nml_fmt::formatter::format_source(&after).ok().as_deref(),
            Some(after.as_str()),
            "{fixture:?} left fmt-dirty by nml fix"
        );
        let second = run("second fix");
        assert!(
            second.contains("0 edit(s) applied"),
            "{fixture:?} not at a fixpoint after one run: {second}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
    assert!(
        checked >= 30,
        "the fmt-clean fixture population moved unexpectedly: {checked}"
    );
}

#[test]
fn fix_converges_a_wide_colliding_file_in_one_run() {
    // Plain same-message findings land TOGETHER (the multiset decrement
    // is per key). Rounds COLLIDE when applied NML2077 repairs
    // un-suppress same-message NML2060s the round also applied — then a
    // failed batch lands ONE candidate per round. Seven restating
    // dependents plus seven suppressed (mid, top-uses-mid,base) pairs
    // need well over eight such rounds; the fixed budget stalled this
    // fully fixable file mid-run, and the scaled budget carries it to
    // the fixpoint in one invocation.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-wide-colliding.nml");
    let mut src = String::from(
        "model spec:\n    x string #sealed\n    y string?\n\nspec base:\n    x = \"1\"\n",
    );
    for i in 1..=7 {
        src.push_str(&format!("\nspec dep{i} uses base:\n    x = \"1\"\n"));
    }
    for i in 1..=7 {
        src.push_str(&format!("\nspec mid{i} uses base:\n    y = \"2\"\n"));
        src.push_str(&format!(
            "\nspec top{i} uses mid{i}, base:\n    x = \"1\"\n"
        ));
    }
    std::fs::write(&f, &src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0 diagnostic(s) not auto-fixable"),
        "one run reaches the fixpoint: {stdout}"
    );
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)] // control characters are invalid in Win32 filenames
#[test]
fn check_output_escapes_hostile_schema_filenames() {
    // The check path's twin of the fix-output rule: `--schema <dir>` is
    // WALKED, so an attributed finding in a hostile-named schema file
    // prints through `report()` — the filename must render escaped, in
    // the primary line and in any note.
    let dir = std::env::temp_dir().join(format!("nml-hostile-schema-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let schema = dir.join("ev\u{1b}]0;pwned\u{7}il.model.nml");
    std::fs::write(&schema, "model spec:\n    x number = \"nope\"\n").unwrap();
    let app = dir.join("app.nml");
    std::fs::write(&app, "spec a:\n    x = 1\n").unwrap();
    let out = nml_bin()
        .args([
            "check",
            "--schema",
            dir.to_str().unwrap(),
            app.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pwned"),
        "the fixture must attribute a finding to the schema file: {stderr}"
    );
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "raw escape bytes must never reach the terminal: {stderr:?}"
    );
    assert!(
        stderr.contains("\\u{1b}"),
        "the hostile byte renders escaped: {stderr}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fix_converges_a_capped_same_key_flood_in_one_run() {
    // The Σ-deficit clause end-to-end (D-A): 129 in-string C0
    // instances of ONE character exceed the 128-diagnostic cap, so the
    // first round can only see (and apply) 128 — and the hidden 129th
    // then SURFACES on the very key the round applied. The old exact
    // decrement read that as a failed round and fell back to
    // one-candidate-per-round, exhausting the budget; the deficit is
    // now charged to the reported suppressed count and the file
    // converges in ONE invocation.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-capped-flood.nml");
    let mut src = String::from("service App:\n");
    for i in 0..129 {
        src.push_str(&format!("    k{i} = \"a\u{1}b\"\n"));
    }
    std::fs::write(&f, &src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("129 edit(s) applied")
            && stdout.contains("0 diagnostic(s) not auto-fixable"),
        "one run lands every instance and reports honestly: {stdout}"
    );
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert_eq!(
        fixed.matches("\"a\\u{1}b\"").count(),
        129,
        "every instance carries the escape"
    );
    assert!(!fixed.contains('\u{1}'), "no raw C0 survives");
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_converges_a_mixed_capped_flood_in_one_run() {
    // Σ-deficit with two keys sharing the cap: 129 of one character
    // push 3 of another (plus their own 129th) past the truncation
    // boundary. The surfacing instances land across BOTH keys over the
    // following rounds; the summary stays honest and the file reaches
    // a clean check in one invocation.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-mixed-capped-flood.nml");
    let mut src = String::from("service App:\n");
    for i in 0..129 {
        src.push_str(&format!("    k{i} = \"a\u{1}b\"\n"));
    }
    for i in 0..3 {
        src.push_str(&format!("    m{i} = \"c\u{2}d\"\n"));
    }
    std::fs::write(&f, &src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("132 edit(s) applied")
            && stdout.contains("0 diagnostic(s) not auto-fixable"),
        "one run lands both keys' instances: {stdout}"
    );
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert_eq!(fixed.matches("\"a\\u{1}b\"").count(), 129, "{stdout}");
    assert_eq!(fixed.matches("\"c\\u{2}d\"").count(), 3, "{stdout}");
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_reports_budget_exhaustion_and_a_second_run_finishes() {
    // The 64-round clamp, exercised for real: 72 restating dependents
    // beside 12 suppressed (mid, top-uses-mid,base) pairs sit safely on
    // the exhaustion plateau (the cliff shapes flip on any fixer
    // improvement — do not shrink this toward (63, 1)). Run 1 exhausts:
    // the stderr note prints EXACTLY once and stdout's summary carries
    // the budget suffix instead of mislabeling landable candidates.
    // Run 2 converges silently; run 3 is the literal fixpoint. Edit
    // counts are fixer-internal and deliberately unasserted.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-budget-exhaustion.nml");
    let mut src = String::from(
        "model spec:\n    x string #sealed\n    y string?\n\nspec base:\n    x = \"1\"\n",
    );
    for i in 1..=72 {
        src.push_str(&format!("\nspec dep{i} uses base:\n    x = \"1\"\n"));
    }
    for i in 1..=12 {
        src.push_str(&format!("\nspec mid{i} uses base:\n    y = \"2\"\n"));
        src.push_str(&format!(
            "\nspec top{i} uses mid{i}, base:\n    x = \"1\"\n"
        ));
    }
    std::fs::write(&f, &src).unwrap();

    let run = || {
        let out = nml_bin()
            .args(["fix", f.to_str().unwrap()])
            .output()
            .expect("failed to run nml");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (stdout, stderr) = run();
    assert_eq!(
        stderr
            .matches("fix round budget reached with fix candidates still standing")
            .count(),
        1,
        "the note prints exactly once: {stderr}"
    );
    assert!(
        stdout.contains("(1 file(s) hit the round budget — run `nml fix` again to continue)"),
        "the summary is honest about the remainder: {stdout}"
    );

    let (stdout, stderr) = run();
    assert!(
        !stderr.contains("fix round budget reached"),
        "run 2 converges without the note: {stderr}"
    );
    assert!(
        stdout.contains("0 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );

    let (stdout, _) = run();
    assert!(
        stdout.contains("0 edit(s) applied across 0 of 1 file(s)"),
        "run 3 is the literal fixpoint: {stdout}"
    );

    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)] // control characters are invalid in Win32 filenames
#[test]
fn fix_output_escapes_hostile_filenames() {
    // A WALKED filename is repo content: an OSC title-set sequence (or a
    // bidi override) in it must never reach the terminal raw through the
    // refusal or summary lines.
    // Guard-owned (r54 NIT #5): a red run must not leave the directory.
    let dir =
        Scratch(std::env::temp_dir().join(format!("nml-hostile-name-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&*dir);
    std::fs::create_dir_all(&*dir).unwrap();
    let f = dir.join("ev\u{1b}]0;pwned\u{7}il.nml");
    // The scalar `.shared` restatement: a printed NoNodeAt refusal.
    let src = concat!(
        "model item:\n    name string+\n    secret string #sealed\n\n",
        "model spec:\n    xs []item #identity\n\n",
        "spec base:\n    xs:\n        - w:\n            secret = \"s\"\n\n",
        "spec over uses base:\n    xs:\n        .secret = \"s\"\n        - w:\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", dir.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fix refused"),
        "the fixture must produce a printed refusal: {stderr}"
    );
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "raw escape bytes must never reach the terminal: {stderr:?}"
    );
    assert!(
        stderr.contains("\\u{1b}"),
        "the hostile byte renders escaped: {stderr}"
    );
}

#[test]
fn fix_never_deletes_a_shared_blocks_distributed_row() {
    // The block-form `.shared` corruption vector: `.retry:` distributes
    // REAL CST rows into every item, so an NML2060 equal-value deletion
    // on ONE item's redundancy would locate the real `max = "3"` row and
    // strip the default from EVERY item — silently, since `nml check`
    // passes afterward. The resolver refuses any target inside a
    // `.shared` body (`SharedDistribution`), printed, file untouched.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-shared-block-distribution.nml");
    let src = concat!(
        "model retry:\n    max string? #sealed\n    mode string?\n\n",
        "model item:\n    name string+\n    retry retry?\n\n",
        "model spec:\n    xs []item #identity\n\n",
        "spec base:\n    xs:\n        - w:\n            retry:\n                max = \"3\"\n\n",
        "spec over uses base:\n    xs:\n        .retry:\n            max = \"3\"\n            mode = \"fast\"\n",
        "        - w:\n        - v:\n",
    );
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        src,
        "a distributed row must never be deleted: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("fix refused: the entry is distributed by its `.shared` block"),
        "the refusal is printed: {stderr}"
    );
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
}

#[test]
fn fix_never_glues_lines_behind_an_unterminated_string() {
    // The stray-quote corruption vector: an unterminated string's token
    // swallows to EOF, and trusting it as "in-string" context flipped
    // every following TRANSPORT CR to content — whose `\r` escape fix
    // then rewrote the old-Mac file's line endings into literal text
    // inside a string value. The reclassifier now excludes tokens named
    // by an unterminated-string error; the file stays byte-identical.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-stray-quote-old-mac.nml");
    let src = "service App:\r    a = \"stray x\r    b = 2\r";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "an uncertain string reading must never grant the in-string fix: {stdout}"
    );
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
}

#[test]
fn fix_applies_the_singular_in_string_escape_and_never_picks_an_alternative() {
    // The D1 taxonomy at the applier: an in-string C0 has ONE
    // value-preserving reading (its escape) — `nml fix` applies it.
    // An in-string NEL has THREE readings (line break | kept byte |
    // mojibake ellipsis) — alternatives are structurally never a sole
    // candidate, so the byte stays until a human chooses; the file's
    // other repair still lands (progress is per-finding, not
    // all-or-nothing).
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-in-string-taxonomy.nml");
    let src = "service App:\n    bell = \"a\u{1}b\"\n    note = \"x\u{85}y\"\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.contains("bell = \"a\\u{1}b\""),
        "the singular escape auto-applies: {fixed:?}\n{stdout}"
    );
    assert!(
        fixed.contains("note = \"x\u{85}y\""),
        "an ambiguous character is never auto-resolved: {fixed:?}\n{stdout}"
    );
    assert!(
        stdout.contains("1 diagnostic(s) not auto-fixable — run `nml check` to see them"),
        "the run counts the standing NEL and points at the next action: {stdout}"
    );
}

#[test]
fn fix_collapses_an_unsound_remove_to_the_escape_and_applies_it() {
    // D-C at the applier: a FEFF separating `""` from `"` inside a
    // multiline body has NO sound removal (deletion would glue a
    // closing `"""` and re-tokenize the rest of the file), so the
    // alternatives COLLAPSE to the singular escape — which, singular,
    // auto-applies. The fixed file parses clean and the value is
    // byte-identical (the escape spells the same character).
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-collapsed-remove.nml");
    let src = "service App:\n    doc = \"\"\"\n        a\n        \"\"\u{FEFF}\"\n        \"\"\"\n    port = 1\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(
        fixed.contains("\"\"\\u{FEFF}\""),
        "exactly the escape lands — never the deletion: {fixed:?}\n{stdout}"
    );
    assert!(!fixed.contains('\u{FEFF}'), "{fixed:?}");
    assert!(stdout.contains("1 edit(s)"), "{stdout}");
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "the fixed file must check clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fix_never_pulls_a_multiline_blank_edge_line_into_the_value() {
    // The r28 corruption vector: a bare CR in a multiline string's
    // ALIGNED blank closing line sat inside the token, so it got the
    // in-string `\r` fix — but `decode_multiline` DROPS blank edge
    // lines from the value, and the applied escape turned the line
    // non-blank and pulled it in: `"body"` became `"body\n\r"` with a
    // clean post-state. Dropped edge lines now grant no repair.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-multiline-blank-edge.nml");
    let src = "service App:\n    doc = \"\"\"\n        body\n        \u{D}\"\"\"\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "a dropped edge line must never gain value bytes: {stdout}"
    );
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
}

#[test]
fn fix_never_reindents_a_value_through_a_blank_middle_line() {
    // The r29 corruption vector: a blank middle line BELOW min-indent
    // is exempt from indent arithmetic; escaping its CR made it
    // participate, dropped min for the whole body, and a second round
    // auto-applied the revealed closing realignment — clean post-state,
    // every value line re-indented. The decode-judged gate refuses the
    // repair, so the file stays byte-identical.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-blank-middle-reindent.nml");
    let src =
        "service App:\n    doc = \"\"\"\n        body\n \u{D} \n        more\n        \"\"\"\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "a geometry-bearing blank line must never gain value bytes: {stdout}"
    );
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
}

#[test]
fn dry_run_diffs_never_echo_raw_control_bytes() {
    // The diff's content lines are repo bytes headed for a terminal —
    // and the very files `fix --dry-run` triages carry raw controls
    // (that is why they have fixes). A raw ESC in the printed `-` line
    // is an ANSI-injection channel (colors, cursor, OSC title); every
    // body byte renders through the sanitizer, escaped and visible.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("dry-run-ansi.nml");
    let src = "service App:\n    x = \"a\u{1B}[31mRED\"\n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", "--dry-run", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("-    x = "),
        "the dry run must show the diff: {stdout}"
    );
    assert!(
        !stdout.contains('\u{1B}'),
        "no raw ESC may reach the terminal: {stdout:?}"
    );
    // The `-` line pins the SANITIZER's own rendering (lowercase, from
    // `escape_default`) — an implementation that stripped the hostile
    // byte instead of escaping it would still show the `+` line's
    // uppercase repair text, so the deleted line is the load-bearing
    // assertion.
    assert!(
        stdout.contains("-    x = \"a\\u{1b}[31mRED\""),
        "the hostile byte renders escaped in the deleted line: {stdout}"
    );
    assert!(
        stdout.contains("\\u{1B}"),
        "the applied repair renders in the added line: {stdout}"
    );
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "a dry run writes nothing"
    );
}

#[test]
fn dry_run_diffs_keep_crlf_terminators_literal() {
    // The r35 sanitizer splits the terminator BEFORE escaping: a CRLF
    // file's dry-run diff keeps every `\r\n` literal (no per-line
    // `\u{D}` noise), while a hostile byte in an UNTERMINATED final
    // line still renders escaped beside the no-newline marker.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("dry-run-crlf.nml");
    let src = "service App:\r\n    a = \"x\u{1}y\"\r\n    b = \"tail\u{1B}!\"";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", "--dry-run", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("-    a = \"x\\u{1}y\"\r\n"),
        "CRLF terminators stay literal, bodies render escaped: {stdout:?}"
    );
    assert!(!stdout.contains("\\u{D}"), "no terminator noise: {stdout}");
    assert!(
        stdout.contains("\\ No newline at end of file"),
        "the unterminated final line carries the marker: {stdout}"
    );
    assert!(!stdout.contains('\u{1B}'), "no raw ESC: {stdout:?}");
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "a dry run writes nothing"
    );
}

#[test]
fn fix_summary_discloses_suppressed_findings_beside_the_count() {
    // The count parenthetical (P1): 128 visible findings and 72 more
    // past the diagnostic limit — the summary must disclose the hidden
    // population beside the number it qualifies (suppressed findings
    // are UNKNOWNS, never folded into the not-auto-fixable count), in
    // the marker row's own vocabulary. The stray-quote swallow shape
    // keeps every finding unfixable, so 0 edits apply.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-suppressed-summary.nml");
    let mut src = String::from("service App:\r    a = \"stray x\r");
    for _ in 0..195 {
        src.push_str("    k = 1\r");
    }
    std::fs::write(&f, &src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "128 diagnostic(s) not auto-fixable (72 more suppressed past \
             the diagnostic limit) — run `nml check` to see them"
        ),
        "{stdout}"
    );
    assert_eq!(std::fs::read(&f).unwrap(), src.as_bytes(), "{stdout}");
    // The dry run keeps the parenthetical (it qualifies the count,
    // which prints) and drops only the pointer tail.
    let out = nml_bin()
        .args(["fix", "--dry-run", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "0 edit(s) would apply across 0 of 1 file(s); 128 diagnostic(s) \
             not auto-fixable (72 more suppressed past the diagnostic limit)"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("run `nml check`"), "{stdout}");
}

#[test]
fn fix_never_rewrites_the_phantom_closing_of_an_unterminated_string() {
    // The NML0020 gate (P2): with no closing delimiter the trailing
    // blank line is a recovery artifact, not the closing quotes — the
    // phantom align-fix used to APPLY, growing the file by 8 bytes of
    // whitespace on a string that has no delimiter to align.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-unterminated-phantom-close.nml");
    let src = "service App:\n    doc = \"\"\"\n        body\n      \n";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        std::fs::read(&f).unwrap(),
        src.as_bytes(),
        "no phantom-close rewrite: {stdout}"
    );
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
}

#[test]
fn a_shared_write_to_a_named_items_typed_field_reaches_and_mismatches() {
    // Q2u (RFC 0025 test plan): a list-wide `.shared` write REACHES a
    // Named item's typed field (a Named key's `name` is lenient, and
    // its other fields are ordinary merge targets), so a type-wrong
    // shared value surfaces as NML2008 against the COMPOSED body — the
    // composed artifact validates like the same body authored plainly.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("q2u-shared-mismatch.nml");
    let src = "\
model itemq:
    name string
    size number

model flowq:
    steps []itemq #identity

flowq base:
    steps:
        - h2:
            size = 1

flowq t uses base:
    steps:
        .size = \"big\"
        - h2:
            name = \"n\"
";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NML2008")
            && stderr.contains("expected number, got string")
            && stderr.contains("'size'"),
        "the shared write reaches the item and mismatches: {stderr}"
    );
}

#[test]
fn fix_leaves_a_cr_terminated_file_byte_identical() {
    // A bare CR in token position has NO machine fix: on a CR-terminated
    // ("old Mac") file every CR is a line ending, and deleting it glues
    // the lines together (`service Api:\r    port = 8080\r` became
    // `service Api:    port = 8080`, and the shrinking finding count
    // ACCEPTED the round). The file is reported, never rewritten.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-cr-terminated.nml");
    let src = "service Api:\r    port = 8080\r";
    std::fs::write(&f, src).unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
    let after = std::fs::read(&f).unwrap();
    assert_eq!(
        after,
        src.as_bytes(),
        "a CR-terminated file must stay byte-identical"
    );
}

#[test]
fn fix_escapes_a_bare_cr_inside_a_string_instead_of_deleting_it() {
    // A bare CR INSIDE a string literal is content: the machine fix is
    // the `\r` escape (value-preserving), never the deletion a CR in
    // token position gets.
    let dir = std::env::temp_dir().join(format!("nml-layers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("fix-cr-in-string.nml");
    std::fs::write(&f, "model m:\n    tag string\n\nm x:\n    tag = \"a\rb\"\n").unwrap();
    let out = nml_bin()
        .args(["fix", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fixed = std::fs::read_to_string(&f).unwrap();
    assert!(fixed.contains("tag = \"a\\rb\""), "{stderr}\n{fixed:?}");
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("failed to run nml");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── RFC 0019 item 0 (step 0d): the shared resolution core in the CLI ──
//
// One golden directory, `tests/fixtures/workspace/expected/`, holds the
// expected output of every scenario with the (machine-specific) canonical
// fixture root spelled `<root>`. THIS file is its only reader — the LSP
// shares the fixture TREE (`tests/fixtures/workspace`, which its harness
// walks) but not these transcripts, and the differential parity harness
// (`tests/integration/parity.rs`) is what holds the two front ends to one
// verdict. `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli` rewrites them:
// review the diff, that IS the change. A golden nobody reads is a file
// that cannot fail, so `every_workspace_golden_is_still_compared` names
// one the day its test goes.

fn fixture(name: &str) -> std::path::PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    workspace_root.join("tests/fixtures").join(name)
}

/// Run `nml` from the repo root: `(exit code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let out = nml_bin().args(args).output().expect("failed to run nml");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The fixture root as the kernel spells it — canonical.
fn canonical_root(name: &str) -> String {
    std::fs::canonicalize(fixture(name))
        .unwrap()
        .display()
        .to_string()
}

/// Compare `actual` (roots normalized to `<root>`) with the golden file.
fn golden(name: &str, actual: &str, roots: &[&str]) {
    let mut normalized = actual.to_string();
    for root in roots {
        normalized = normalized.replace(root, "<root>");
    }
    let path = fixture("workspace/expected").join(format!("{name}.txt"));
    if std::env::var_os("NML_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &normalized).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "golden {} unreadable ({e}) — NML_UPDATE_GOLDEN=1 to create",
            path.display()
        )
    });
    assert_eq!(
        normalized, expected,
        "golden {name} drifted (NML_UPDATE_GOLDEN=1 to accept)"
    );
}

/// Every transcript in the golden directory is still COMPARED by a test
/// here. A golden whose test was renamed or deleted keeps passing — it
/// is simply never read — so the record grows files that can no longer
/// fail; the same shape `every_composition_golden_line_is_live` closes
/// for `compose.golden`. Source-based, like that one: the call site is
/// `golden("<name>", …)`, and this file is its own corpus.
#[test]
fn every_workspace_golden_is_still_compared() {
    const THIS: &str = include_str!("cli_tests.rs");
    let dir = fixture("workspace/expected");
    let mut orphans: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for entry in std::fs::read_dir(&dir).expect("the golden directory") {
        let path = entry.expect("a golden entry").path();
        if path.extension().is_none_or(|x| x != "txt") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a utf-8 stem")
            .to_string();
        seen += 1;
        if !THIS.contains(&format!("golden(\n        \"{stem}\""))
            && !THIS.contains(&format!("golden(\"{stem}\""))
        {
            orphans.push(stem);
        }
    }
    assert!(seen > 0, "no goldens under {}", dir.display());
    assert!(
        orphans.is_empty(),
        "golden transcript(s) no test compares any more — delete them, or restore the test \
         that read them: {orphans:?}"
    );
}

fn check_in_workspace(file: &str) -> (i32, String, String) {
    run([
        "check",
        "--root",
        "tests/fixtures/workspace",
        &format!("tests/fixtures/workspace/{file}"),
    ]
    .as_slice())
}

#[test]
fn check_denies_composition_without_grant_2064() {
    let (code, stdout, stderr) = check_in_workspace("tenants/cu/member-lookup.flow.nml");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2064]"), "{stderr}");
    assert!(
        stderr.contains("binding 'tenantFlows' (demo.package.nml) carries no `layers:` grant"),
        "{stderr}"
    );
    // The tenant's `nml-project.nml` on the file's path is inert (NML2080)
    // and listed as a note; the file still binds to the operator's binding.
    assert!(stderr.contains("warning[NML2080]"), "{stderr}");
    assert!(stderr.contains("tenants/cu/nml-project.nml"), "{stderr}");
    // RFC 0026 B-1: the remedy is a LOCATED note at the binding in its
    // manifest, naming the exact key to admit.
    assert!(
        stderr.contains(
            "demo.package.nml:10:7: note: to permit it, give this binding a `layers:` grant whose \
             `allowRefs` admits \"tenants/cu/member-lookup.flow.nml\"\n"
        ),
        "{stderr}"
    );
    golden(
        "check-no-grant",
        &format!("{stdout}{stderr}"),
        &[&canonical_root("workspace")],
    );
}

#[test]
fn check_ambiguous_claim_is_denied_before_the_file_is_read() {
    // An ambiguously-claimed file is denied by the universe (r51 #3):
    // the error names both claimants and the file is never read, so the
    // composing file no longer reaches NML2064 — one denial, one shape,
    // whether or not the file composes.
    let (code, stdout, stderr) = check_in_workspace("shared/x.flow.nml");
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "shared/x.flow.nml: error[NML2087]: 2 manifests claim this file: demo.package.nml (shared, \
             files[0] = \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \
             \"shared/**/*.flow.nml\") — an ambiguously-claimed file is denied"
        ),
        "{stderr}"
    );
    assert!(
        !stderr.contains("NML2064"),
        "denied before composing: {stderr}"
    );
    golden(
        "check-ambiguous",
        &format!("{stdout}{stderr}"),
        &[&canonical_root("workspace")],
    );
}

#[test]
fn check_closed_unbound_names_the_claim_count() {
    let (code, stdout, stderr) = check_in_workspace("docs/unclaimed.nml");
    assert_eq!(code, 1, "{stderr}");
    // r89 (D8): the root is the run's fact (the closing row, `binding`'s
    // `root` line), never embedded in a per-file kernel sentence.
    let root = canonical_root("workspace");
    assert!(
        stderr.contains(
            "no binding governs this file in the closed universe (2 manifest(s) discovered)"
        ) && !stderr.contains(&root),
        "{stderr}"
    );
    golden(
        "check-closed-unbound",
        &format!("{stdout}{stderr}"),
        &[&root],
    );
}

#[test]
fn check_binding_validator_judges_bound_file() {
    // D-0d-1: with no --schema, a claimed file validates under its
    // binding's package — `thing` comes from the manifest's core.model.nml,
    // and the binding's `strict = true` is what rejects the unknown field.
    let (code, stdout, stderr) = check_in_workspace("tenants/cu/plain.flow.nml");
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok (1 declaration(s))"), "{stdout}");
    let (code, _, stderr) = check_in_workspace("tenants/cu/bad.flow.nml");
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("[NML2004]") || stderr.contains("expected string"),
        "{stderr}"
    );
    assert!(
        stderr.contains("'w'"),
        "strict binding rejects the unknown field: {stderr}"
    );
}

#[test]
fn schema_flag_conflicts_with_governing_binding() {
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace",
        "--schema",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/plain.flow.nml",
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains(
            "--schema tests/fixtures/workspace conflicts with the manifest that governs this file"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("binding 'tenantFlows' of demo.package.nml claims it (files[0] = \"tenants/**/*.flow.nml\")"),
        "{stderr}"
    );
    assert!(
        stderr.contains("run `nml binding tests/fixtures/workspace/tenants/cu/plain.flow.nml`"),
        "{stderr}"
    );
    golden(
        "check-schema-conflict",
        &format!("{stdout}{stderr}"),
        &[&canonical_root("workspace")],
    );
    // `nml fix` obeys the same rule.
    let (code, _, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        "tests/fixtures/workspace",
        "--schema",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/plain.flow.nml",
    ]);
    assert_eq!(code, 2, "{stderr}");
}

#[test]
fn check_open_repo_still_composes() {
    // No manifest anywhere within the fence: the open developer context —
    // composition permitted, exactly as before item 0 — with and without
    // an explicit root.
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace-open",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok (3 declaration(s))"), "{stdout}");
    let (code, stdout, stderr) = run(&["check", "tests/fixtures/workspace-open/x.nml"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok (3 declaration(s))"), "{stdout}");
}

#[test]
fn root_flag_pins_universe_and_rejects_outside_target() {
    // A target outside the root is the invocation's mistake: exit 2 in
    // every verb (r85 D3; it was a per-target failure, exit 1, here).
    let (code, _, stderr) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("is outside the workspace root"), "{stderr}");
    assert!(
        stderr.contains("(--root)"),
        "the uniform origin tag: {stderr}"
    );
    // The universe is the flag's: the same file under the open root composes.
    let (code, _, _) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace-open",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(code, 0);
    // A root that is not a directory is a usage-level error.
    let (code, _, stderr) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace/demo.package.nml",
        "tests/fixtures/workspace/docs/unclaimed.nml",
    ]);
    assert_eq!(code, 2, "the invocation's mistake, exit 2: {stderr}");
    assert!(
        stderr.contains("--root tests/fixtures/workspace/demo.package.nml: not a directory"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn check_symlinked_content_under_closed_binding_2083() {
    let root = canonical_root("workspace-link-a");
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        "tests/fixtures/workspace-link-a",
        "tests/fixtures/workspace-link-a/tenants/cu/lib/base.flow.nml",
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2083]"), "{stderr}");
    assert!(
        stderr.contains("path component `lib` is a symlink"),
        "{stderr}"
    );
    assert!(!stderr.contains("vendor"), "never the target: {stderr}");
    golden("check-symlink", &format!("{stdout}{stderr}"), &[&root]);
}

#[cfg(unix)]
#[test]
fn symlink_target_existence_is_not_observable() {
    // Two fixture roots differing ONLY in whether `tenants/cu/lib`'s
    // target exists: byte-identical stdout and stderr, and the same exit
    // code — a planted link is not an existence oracle. (The file is
    // never opened: in `link-b` it does not even exist.)
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        let root = canonical_root(&format!("workspace-link-{v}"));
        let (code, stdout, stderr) = run(&[
            "check",
            "--root",
            &format!("tests/fixtures/workspace-link-{v}"),
            &format!("tests/fixtures/workspace-link-{v}/tenants/cu/lib/base.flow.nml"),
        ]);
        let normalized = format!("{code}\n{stdout}{stderr}")
            .replace(&root, "<root>")
            .replace(&format!("workspace-link-{v}"), "workspace-link-<v>");
        outputs.push(normalized);
    }
    assert_eq!(outputs[0], outputs[1]);
    assert!(outputs[0].starts_with("1\n"));
    // `nml binding` too.
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        let root = canonical_root(&format!("workspace-link-{v}"));
        let (code, stdout, stderr) = run(&[
            "binding",
            "--root",
            &format!("tests/fixtures/workspace-link-{v}"),
            &format!("tests/fixtures/workspace-link-{v}/tenants/cu/lib/base.flow.nml"),
        ]);
        outputs.push(
            format!("{code}\n{stdout}{stderr}")
                .replace(&root, "<root>")
                .replace(&format!("workspace-link-{v}"), "workspace-link-<v>"),
        );
    }
    assert_eq!(outputs[0], outputs[1]);
}

#[test]
fn binding_prints_governing_and_indices() {
    let root = canonical_root("workspace");
    let (code, stdout, stderr) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("file      tenants/cu/member-lookup.flow.nml\n"),
        "{stdout}"
    );
    // r88 (P5): spelled as typed from the repository root.
    assert!(
        stdout.contains("root      tests/fixtures/workspace  (--root)\n"),
        "{stdout} (canonical {root})"
    );
    assert!(
        stdout.contains("binding   tenantFlows   demo blake3:"),
        "{stdout}"
    );
    assert!(
        stdout.contains(", workspace manifest (demo.package.nml)\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "anchor    .   matched files[0] = \"tenants/**/*.flow.nml\"   (auto-associated)\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("layers    none — composition denied (NML2064)\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("notes     tenants/cu/nml-project.nml: warning[NML2080]"),
        "{stdout}"
    );
    golden("binding-bound", &stdout, &[&root]);

    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/shared/x.flow.nml",
    ]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains("binding   AMBIGUOUS — 2 manifests claim this file: demo.package.nml (shared, files[0] = \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \"shared/**/*.flow.nml\")\n"),
        "{stdout}"
    );
    golden("binding-ambiguous", &stdout, &[&root]);

    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/docs/unclaimed.nml",
    ]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains(
            "binding   none — closed universe (2 manifest(s) discovered); no files glob claims this file\n"
        ),
        "{stdout}"
    );
    golden("binding-unbound", &stdout, &[&root]);

    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace-open",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(code, 1);
    // The two branches of ONE row name the two values of ONE field: the
    // `--json` `universe` key, which carries exactly `open` / `closed`. The
    // open branch used to say "open context" while the closed one said
    // "closed universe" — two nouns for one field, across the CLI, the wire
    // and the editor's tooltip.
    assert!(
        stdout.contains(
            "binding   none — open universe (no manifest within the fence); composition permitted\n"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("open context"), "{stdout}");
}

#[test]
fn binding_exit_codes() {
    // 0 bound, 1 unbound/ambiguous (above), 2 error: a missing file, an
    // outside-root file, a bad root.
    let (code, _, stderr) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/nope.flow.nml",
    ]);
    // A missing file still has a key and a would-be binding: the verb
    // answers the question asked (what WOULD govern it) — bound, exit 0.
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("is outside the workspace root"), "{stderr}");
    let (code, _, stderr) = run(&["binding", "--root", "no/such/dir", "x.nml"]);
    assert_eq!(code, 2, "{stderr}");
    let (code, _, stderr) = run(&["binding"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("usage: nml binding"), "{stderr}");
}

// ── RFC 0019 item 0, E28 (r50 fold): the universe never trusts an
// author's link, a truncation is an error, an inert manifest is inert ──

/// A scratch directory that removes itself on EVERY path — a failed
/// assertion included (a leftover `target/tmp/*` from a red run was the
/// r51 evidence that a success-path cleanup is none).
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A stray token where a declaration starts recovers as a declaration with
/// no name; two of them once reported `duplicate declaration ''` at 1:1 —
/// FIRST, ahead of the parse finding that names the stray token, and under
/// a manifest the only row `nml check` showed. A nameless declaration is no
/// declaration: the parse findings stand alone, `explain` points at them.
#[test]
fn a_nameless_recovered_declaration_is_never_a_duplicate() {
    let dir = scratch_dir("nameless-decl");
    let file = dir.join("f.nml");
    std::fs::write(&file, "thing a:\n    v = \"x\"\n\\n[]b c:\\n[]d e:\n").unwrap();
    let target = file.display().to_string();
    let (code, _stdout, stderr) = run(&["parse", &target]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!stderr.contains("NML1000"), "{stderr}");
    assert!(!stderr.contains("''"), "{stderr}");
    assert!(
        stderr.contains("3:1: error[NML0004]: unexpected character `\\`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML0004"),
        "{stderr}"
    );
    // The same text as a manifest: the row `check` shows is the parse finding, at its line.
    let ws = scratch_dir("nameless-decl-manifest");
    std::fs::create_dir_all(ws.join("tenants/cu")).unwrap();
    std::fs::write(ws.join("core.model.nml"), "model core:\n    v string\n").unwrap();
    std::fs::write(
        ws.join("tenants/cu/plain.flow.nml"),
        "core a:\n    v = \"x\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("demo.package.nml"),
        "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
         file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        files:\n            \
         - \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n\\n[]directive directives:\\n    - live:\n",
    )
    .unwrap();
    let root = ws.to_str().unwrap().to_string();
    let target = ws.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, _stdout, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!stderr.contains("''"), "{stderr}");
    assert!(
        stderr.contains("demo.package.nml:15:1: error[NML2088]: manifest failed to load: unexpected character `\\`"),
        "{stderr}"
    );
}

/// A scratch directory under `CARGO_TARGET_TMPDIR` (inside the checkout —
/// under the repository's own `.git` fence when there is one).
fn scratch_dir(tag: &str) -> Scratch {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e28-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

/// A scratch directory OUTSIDE the checkout — under the system temp
/// directory, canonicalized — with no `.git` entry of any kind in it or
/// above it, ASSERTED rather than assumed (the message names the fence
/// if the environment has one), for the one derivation the checkout
/// cannot host: a target with no VCS fence at all (E21's narrowing to
/// the target's own directory). A pin that asked this of a directory
/// under `CARGO_TARGET_TMPDIR` held only in a copy of the tree with no
/// `.git` and failed in every clone.
fn unfenced_temp_dir(tag: &str) -> Scratch {
    let dir = system_temp_dir(tag);
    for ancestor in dir.ancestors() {
        assert!(
            std::fs::symlink_metadata(ancestor.join(".git")).is_err(),
            "the system temp directory is under a VCS fence at {} — this pin needs an \
             unfenced directory",
            ancestor.display()
        );
    }
    dir
}

/// A scratch directory under the SYSTEM temp directory (canonicalized —
/// macOS `/var` is `/private/var`), outside the checkout: the no-VCS
/// regime's home, fenced or not as the environment has it.
fn system_temp_dir(tag: &str) -> Scratch {
    let base = std::env::temp_dir().canonicalize().unwrap();
    let dir = base.join(format!("nml-e28-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

/// Run `nml` from `dir` (a relative target is resolved against it):
/// `(exit code, stdout, stderr)`.
fn run_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = nml_bin()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run nml");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Copy a fixture tree verbatim — files, directories, and symlinks AS
/// symlinks (the link fixtures are what they are because of the link).
fn copy_tree(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&from).unwrap();
        if meta.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&from).unwrap(), &to).unwrap();
        } else if meta.is_dir() {
            std::fs::create_dir_all(&to).unwrap();
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Whether a `.git` entry of any kind sits on `dir`'s ancestor chain —
/// the E21 fence regime the derived root runs under.
#[cfg(unix)]
fn vcs_fenced(dir: &Path) -> bool {
    dir.ancestors()
        .any(|a| std::fs::symlink_metadata(a.join(".git")).is_ok())
}

/// Run `nml` with a watchdog: a hang is a failure, not a CI timeout —
/// and the hung child is killed and reaped BEFORE the failure is raised,
/// so a red run never leaves an `nml` blocked on a FIFO behind it (r51
/// #6: under mutation the child lingered until the driver killed it).
/// Both pipes are drained on threads while the watchdog polls (r52 #10):
/// a child writing past the pipe's capacity would otherwise block on the
/// write and read as a hang.
fn run_bounded(args: &[&str], seconds: u64) -> (i32, String, String) {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = nml_bin()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run nml");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let drain_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let drain_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = drain_out.join().expect("stdout drained");
            let err = drain_err.join().expect("stderr drained");
            return (
                status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out).into_owned(),
                String::from_utf8_lossy(&err).into_owned(),
            );
        }
        if started.elapsed() >= std::time::Duration::from_secs(seconds) {
            let _ = child.kill();
            let _ = child.wait();
            // The pipes close with the child, so the drains end.
            let _ = drain_out.join();
            let _ = drain_err.join();
            panic!("nml {args:?} hung for {seconds}s");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// `check` and `binding` on the link fixture at `root` WITHOUT `--root`,
/// normalized: `(exit, stdout+stderr)` per verb.
#[cfg(unix)]
fn derived_link_outputs(root: &Path, variant: &str) -> Vec<String> {
    let canonical = std::fs::canonicalize(root).unwrap().display().to_string();
    let file = root.join("tenants/cu/lib/base.flow.nml");
    ["check", "binding"]
        .iter()
        .map(|verb| {
            let (code, stdout, stderr) = run(&[verb, file.to_str().unwrap()]);
            format!("{code}\n{stdout}{stderr}")
                .replace(&canonical, "<root>")
                .replace(root.to_str().unwrap(), "<root>")
                .replace(variant, "<v>")
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn derived_root_never_resolves_an_author_link() {
    // E28 (2), the DEFAULT invocation: no `--root`. The two link fixtures
    // differ only in whether `tenants/cu/lib`'s target exists; the
    // derived root must not depend on it — byte-identical output, exit
    // 1, never the target's name. Which rejection fires depends on the
    // E21 regime the checkout runs under: inside a `.git` fence the
    // universe derives from the manifest's directory and the link is
    // NML2083; with no VCS root the fence would be the link itself, and
    // derivation refuses (pass `--root`). Pre-fix: the root derived
    // from the link's target — `ok` for the existing target with no VCS,
    // "not a directory" for the dangling one.
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        outputs.push(derived_link_outputs(
            &fixture(&format!("workspace-link-{v}")),
            &format!("workspace-link-{v}"),
        ));
    }
    assert_eq!(outputs[0], outputs[1]);
    for out in &outputs[0] {
        assert!(!out.contains("vendor"), "never the target: {out}");
        assert!(!out.contains(": ok"), "{out}");
    }
    let (check, binding) = (&outputs[0][0], &outputs[0][1]);
    assert!(check.starts_with("1\n"), "{check}");
    if vcs_fenced(&fixture("workspace-link-a")) {
        assert!(check.contains("error[NML2083]"), "{check}");
        assert!(
            check.contains("path component `lib` is a symlink"),
            "{check}"
        );
        assert!(
            binding.contains("(derived within the .git fence"),
            "{binding}"
        );
    } else {
        assert!(check.contains("cannot derive a workspace root"), "{check}");
        assert!(check.contains("directory `lib` is a symlink"), "{check}");
        assert!(check.contains("--root"), "{check}");
        assert!(binding.starts_with("2\n"), "{binding}");
    }
}

#[cfg(unix)]
#[test]
fn derived_root_under_a_git_fence_rejects_symlinked_content_2083() {
    // The fenced regime, pinned deterministically: each link fixture
    // copied under a fresh `.git`. Byte-identical across target
    // existence for `check` AND `binding`, NML2083 naming `lib`, the
    // origin tag saying the root was derived within the fence.
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        let dir = scratch_dir(&format!("fence-{v}"));
        copy_tree(&fixture(&format!("workspace-link-{v}")), &dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        outputs.push(derived_link_outputs(&dir, &format!("fence-{v}")));
    }
    assert_eq!(outputs[0], outputs[1]);
    let (check, binding) = (&outputs[0][0], &outputs[0][1]);
    assert!(check.starts_with("1\n"), "{check}");
    assert!(check.contains("error[NML2083]"), "{check}");
    assert!(
        check.contains("path component `lib` is a symlink"),
        "{check}"
    );
    assert!(!check.contains("vendor"), "{check}");
    // r88 (P5): the root is spelled from the working directory when the
    // scratch sits under it (`target/tmp/…`), canonical (`<root>`) else.
    assert!(
        binding
            .lines()
            .any(|l| l.starts_with("root      ") && l.contains("  (derived within the .git fence")),
        "{binding}"
    );
    assert!(binding.contains("error[NML2083]"), "{binding}");
}

#[cfg(unix)]
#[test]
fn derived_root_without_vcs_refuses_a_symlinked_target_directory() {
    // The no-VCS regime (the system temp dir, outside any checkout): E21
    // fences at the target's own directory, which IS the link — derive
    // refuses with the `--root` hint, identically whether the link's
    // target exists, and never says `ok`. (Should the temp dir sit under
    // a `.git` after all, the fenced regime's NML2083 is the output.)
    let mut outputs = Vec::new();
    let mut fenced = false;
    for v in ["a", "b"] {
        let dir = system_temp_dir(&format!("novcs-{v}"));
        copy_tree(&fixture(&format!("workspace-link-{v}")), &dir);
        fenced = vcs_fenced(&dir);
        outputs.push(derived_link_outputs(&dir, &format!("novcs-{v}")));
    }
    assert_eq!(outputs[0], outputs[1]);
    let check = &outputs[0][0];
    assert!(check.starts_with("1\n"), "{check}");
    assert!(
        !check.contains("vendor") && !check.contains(": ok"),
        "{check}"
    );
    if fenced {
        assert!(check.contains("error[NML2083]"), "{check}");
    } else {
        assert!(check.contains("cannot derive a workspace root"), "{check}");
        assert!(
            check.contains(
                "the target's directory `lib` is a symlink and no VCS root fences the walk"
            ),
            "{check}"
        );
        assert!(check.contains("(pass --root <dir>)"), "{check}");
    }
}

/// A scratch copy of the `workspace` fixture (goldens left out).
fn workspace_copy(tag: &str) -> Scratch {
    let dir = scratch_dir(tag);
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    dir
}

fn check_under(dir: &Path, file: &str) -> (i32, String, String) {
    run(&[
        "check",
        "--root",
        dir.to_str().unwrap(),
        dir.join(file).to_str().unwrap(),
    ])
}

#[test]
fn deep_tenant_directory_chain_never_switches_validation_off() {
    // E28 (1): a tenant commits a 64-deep directory chain under the
    // strict `tenantFlows` binding. Pre-fix the walk reported a
    // truncation WARNING, the universe closed, the file resolved Unbound
    // and validated parse-only: `bad.flow.nml: ok`, exit 0. Now the
    // chain is an exact skip and the binding keeps governing.
    let dir = workspace_copy("deep");
    let chain: std::path::PathBuf = (1..=64).map(|i| format!("a{i}")).collect();
    std::fs::create_dir_all(dir.join("tenants/cu").join(&chain)).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/bad.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(!stderr.contains("cannot enumerate"), "{stderr}");
    assert!(
        stderr.contains("error[NML"),
        "validated under the binding: {stderr}"
    );
    assert!(!stdout.contains("ok"), "{stdout}");
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    assert!(!stderr.contains("cannot enumerate"), "{stderr}");
}

/// E28 (1) + the A16 amendment (r73): past the entry bound the walk
/// stops. The fixture's `tenants/**/*.flow.nml` makes `tenants/cu` a
/// BUDGET UNIT, so the tenant that spends it is denied in full — every
/// verb reports an ERROR naming the unit and the stop directory and
/// exits 1, `check` never reads the file, `fix` rewrites nothing — and
/// the sibling tenant `tenants/du` still validates green in the SAME
/// run of the SAME universe. (Pre-r73 this spam truncated the whole
/// universe; the operator's every file failed with it.)
///
/// The CLI at the REAL bound: a smaller injectable bound would be a
/// second code path this test no longer proves, so it creates
/// `MAX_ENTRIES + 1` files — the loop follows the constant rather than
/// restating 65,536. Measured 14.4 s of `File::create` on a loaded
/// APFS host (8 s quiet) plus 18.8 s to remove; `windows-latest` NTFS
/// with Defender is plausibly 1–3 min. PERF TIER since r73 (proposal
/// 2): the DEFAULT lane keeps the mock-kernel bound pin
/// (`unit_budget_exhaustion_closes_only_that_unit`, `0..=MAX_ENTRIES`
/// over the scripted FS) and the NML2089 sentence pin (`diag`), so the
/// only thing this drive adds — that the real `StdFs` walk reaches the
/// bound — is also the only thing it costs minutes for. `StdFs::
/// list_dir` has no per-OS branch, so the Linux-only lane loses no
/// per-platform coverage.
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_` \
            (creates MAX_ENTRIES + 1 files: ≈ 8–15 s on APFS, 1–3 min plausible on NTFS)"]
fn perf_entry_bound_truncation_denies_only_the_tenant_unit() {
    let dir = workspace_copy("entries");
    // A sibling tenant, to prove the denial stops at the unit boundary.
    std::fs::create_dir_all(dir.join("tenants/du")).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        dir.join("tenants/du/plain.flow.nml"),
    )
    .unwrap();
    let spam = dir.join("tenants/cu/spam");
    std::fs::create_dir_all(&spam).unwrap();
    for i in 0..=nml_validate::workspace::MAX_ENTRIES {
        std::fs::File::create(spam.join(format!("f{i}"))).unwrap();
    }
    let denial = "the discovery budget for `tenants/cu` is exhausted";
    let stop = "the walk stopped at `tenants/cu/spam`";
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "tenants/cu/plain.flow.nml: error[NML2089]: {denial}"
        )),
        "{stderr}"
    );
    assert!(stderr.contains(stop), "{stderr}");
    assert!(
        stderr.contains("files outside `tenants/cu` are unaffected"),
        "{stderr}"
    );
    // The unit denial is a per-KEY finding, not a universe error: the
    // CLI's `--root` advice (which rides the universe row) is absent,
    // and rooting inside the unit would re-open the universe anyway.
    assert!(
        !stderr.contains("pass --root to a smaller tree"),
        "{stderr}"
    );
    assert!(!stderr.contains("cannot enumerate manifests"), "{stderr}");
    assert!(stderr.trim_end().ends_with("error: 1 error(s)"), "{stderr}");
    assert!(!stdout.contains("ok"), "{stdout}");
    // r74 MERGE — the run's closing row (r73-cli claims 1 and 4) states
    // the unit truncation as DATA. The universe is still CLOSED and its
    // manifests are still counted — the operator's claim that induced the
    // unit stands above it, so `Closure::Complete` holds — which is
    // exactly why `closed`/`manifests` alone would read as a whole
    // universe: `truncatedUnits` names the unit and its stop directory,
    // the fact a consumer could otherwise learn only by parsing the
    // NML2089 sentence. The denial itself is a `diagnostic` row and the
    // process exit rides `exit`.
    let root = dir.to_str().unwrap();
    let denied = dir.join("tenants/cu/plain.flow.nml");
    let unit =
        serde_json::json!([{"unit": "tenants/cu", "stop": "tenants/cu/spam", "why": "entries"}]);
    let (code, rows) = json_rows(&["check", "--json", "--root", root, denied.to_str().unwrap()]);
    assert_eq!(code, 1, "{rows:?}");
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary", "{last}");
    assert_eq!(last["exit"], 1, "{last}");
    assert_eq!(last["universe"], "closed", "{last}");
    assert!(last["manifests"].as_u64().unwrap() >= 1, "{last}");
    assert_eq!(last["truncatedUnits"], unit, "{last}");
    assert!(
        rows.iter()
            .any(|r| r["type"] == "diagnostic" && r["code"] == "NML2089"),
        "the denial is a diagnostic row: {rows:?}"
    );
    // THE amendment: the sibling tenant is untouched, in the same
    // universe, in the same walk.
    let (code, stdout, stderr) = check_under(&dir, "tenants/du/plain.flow.nml");
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    assert!(!stderr.contains("NML2089"), "{stderr}");
    // And so is the operator's own content elsewhere: `shared/x.flow.nml`
    // keeps the verdict it has WITHOUT the spam — the fixture's two
    // manifests claim it, so rule 3 denies it (NML2087) exactly as
    // before. The unit's denial does not reach it, and no universe row
    // appears on it.
    let (code, stdout, stderr) = check_under(&dir, "shared/x.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains("error[NML2087]"), "{stderr}");
    assert!(!stderr.contains("NML2089"), "{stderr}");
    let (code, stdout, stderr) = run(&[
        "validate",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains(denial), "{stderr}");
    // `fix` rewrites NOTHING, prints the denial, and the denied path
    // could not be fixed — exit 1, exactly as an NML2083-rejected path
    // (link-matrix rows F1–F3) and an absent path: a per-key denial goes
    // through `report_universe`'s finding tally and counts as a failed
    // path, where a WHOLE-universe truncation is refused up front.
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/bad.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    assert!(stderr.contains(denial), "{stderr}");
    assert!(
        stdout.contains("0 edit(s) would apply across 0 of 1 file(s)"),
        "nothing is fixed: {stdout}"
    );
    // No NML2080 noise about inputs inside the denied unit: the walk
    // disowned the subtree, notes included.
    assert!(!stderr.contains("NML2080"), "{stderr}");
    // r74 MERGE — the merged exit policy, pinned in BOTH surfaces. Every
    // exit routes through the run's one `finish()` (r73-cli claim 1), so
    // the closing row's `exit` IS the fixer's 0 above, the denial is
    // counted in `errors`, `remaining` and `failed` (the denied path
    // could not be fixed: exit 1, as an absent path), and the row still
    // states the truncated unit; `--check` is a TRUE gate over the same
    // denial.
    let bad = dir.join("tenants/cu/bad.flow.nml");
    let (code, rows) = json_rows(&[
        "fix",
        "--dry-run",
        "--json",
        "--root",
        root,
        bad.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{rows:?}");
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary", "{last}");
    assert_eq!(last["verb"], "fix", "{last}");
    assert_eq!(last["exit"], 1, "{last}");
    assert_eq!(last["edits"], 0, "{last}");
    assert_eq!(last["remaining"], 1, "{last}");
    assert_eq!(last["errors"], 1, "{last}");
    assert_eq!(last["failed"], 1, "{last}");
    assert_eq!(last["truncatedUnits"], unit, "{last}");
    let (code, stdout, stderr) = run(&["fix", "--check", "--root", root, bad.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains(denial), "{stderr}");
    assert!(!stderr.contains("would apply"), "{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "the denied path could not be fixed, under the gate too: {stderr}"
    );
    assert!(
        stdout.contains("0 edit(s) would apply across 0 of 1 file(s)"),
        "{stdout}"
    );
    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    // r89 (D8): the closed form names the claim count; the root is the
    // block's own `root` line.
    assert!(
        stdout.contains("binding   none — closed universe ("),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "notes     tenants/cu/plain.flow.nml: error[NML2089]: {denial}"
        )),
        "{stdout}"
    );
}

#[test]
fn inert_tenant_manifest_never_fails_other_checks() {
    // E28 (4a): a malformed manifest a tenant commits inside claimed
    // content is inert — never loaded — so the operator's other checks
    // stay green (pre-fix: "manifest failed to load", exit 1, for every
    // file in the repository).
    let dir = workspace_copy("inert");
    std::fs::write(
        dir.join("tenants/cu/evil.package.nml"),
        "package evil:\n    this is not a manifest\n",
    )
    .unwrap();
    let (code, stdout, stderr) = check_under(&dir, "vendor/base.flow.nml");
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    assert!(!stderr.contains("failed to load"), "{stderr}");
    let (code, _, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr
            .contains("warning[NML2080]: package manifest `tenants/cu/evil.package.nml` is inert"),
        "{stderr}"
    );
    assert!(!stderr.contains("failed to load"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn symlinked_fifo_source_is_refused_without_hanging() {
    // E28 (4b): a LIVE manifest's declared source replaced by a symlink
    // to a FIFO. Pre-fix discovery opened it and hung (a tenant DoS of
    // the operator's CI); now the kind is checked through the oracle
    // before any read — refused fast, typed, never opened.
    let dir = workspace_copy("fifo");
    let pipe = dir.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("mkfifo runs");
    assert!(status.success());
    std::fs::remove_file(dir.join("core.model.nml")).unwrap();
    std::os::unix::fs::symlink(&pipe, dir.join("core.model.nml")).unwrap();
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_bounded(
        &[
            "check",
            "--root",
            dir.to_str().unwrap(),
            dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
        ],
        20,
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("demo.package.nml: error[NML2088]: manifest failed to load: declared source `core.model.nml` (schemas[0].file in `demo.package.nml`) is unavailable: a symlink — a declared source is never read through a link; replace the link with the file itself"),
        "{stderr}"
    );
}

/// One comment line of `n` bytes: the cheapest padding the parser knows
/// (a comment LINE costs the quadratic parser; one long line does not),
/// so a cap test measures the cap, never the parse.
fn comment_padding(n: usize) -> String {
    format!("// {}\n", "x".repeat(n))
}

#[test]
fn oversized_manifest_is_refused_by_the_byte_cap() {
    // The manifest cap (256 KiB, r51 #1): a live manifest past it is a
    // load error naming the KIND and the bound — never parsed. Pre-fix
    // the one 4 MiB cap covered every kind, so a 300 KiB manifest
    // loaded, was parsed on every invocation, and the check passed.
    let dir = workspace_copy("bigmanifest");
    let mut big = std::fs::read_to_string(dir.join("other.package.nml")).unwrap();
    big.push_str(&comment_padding(256 * 1024));
    std::fs::write(dir.join("other.package.nml"), big).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    // r69b: the manifest is named by the row's source and the message is
    // the reader's refusal — never `declared source 'other.package.nml'`,
    // the manifest wrapped as its own declared source (pre-fold).
    assert!(
        stderr.contains("other.package.nml: error[NML2088]: manifest failed to load: too large: over 256 KiB (262400 bytes) — a package manifest is read only up to 256 KiB (262144 bytes)"),
        "{stderr}"
    );
    assert!(!stderr.contains("declared source"), "{stderr}");
    assert!(!stdout.contains("ok"), "{stdout}");
}

#[test]
fn oversized_project_config_is_refused_by_the_byte_cap() {
    // The same 256 KiB cap for a live project config (r51 #1): refused
    // naming the kind; pre-fix a 300 KiB config loaded under the 4 MiB
    // cap and the check passed.
    let dir = workspace_copy("bigconfig");
    let mut big = String::from("project p:\n    autoAssociate = true\n");
    big.push_str(&comment_padding(256 * 1024));
    std::fs::write(dir.join("nml-project.nml"), big).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("nml-project.nml: error[NML2088]: project config failed to load: too large: over 256 KiB (262184 bytes) — a project config is read only up to 256 KiB (262144 bytes)"),
        "{stderr}"
    );
    assert!(!stdout.contains("ok"), "{stdout}");
}

#[test]
fn declared_source_reads_under_its_own_cap() {
    // The split (r51 #1): a declared schema source keeps the 4 MiB cap —
    // a 300 KiB source (over the manifest cap) loads and the file it
    // governs validates; past 4 MiB it is refused naming the SOURCE cap
    // (pre-fix the refusal named "a resolution input").
    let dir = workspace_copy("bigsource");
    let mut core = std::fs::read_to_string(dir.join("core.model.nml")).unwrap();
    core.push_str(&comment_padding(300 * 1024));
    std::fs::write(dir.join("core.model.nml"), &core).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    assert!(!stderr.contains("byte bound"), "{stderr}");
    core.push_str(&comment_padding(4 * 1024 * 1024));
    std::fs::write(dir.join("core.model.nml"), &core).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("demo.package.nml: error[NML2088]: manifest failed to load: declared source `core.model.nml` (schemas[0].file in `demo.package.nml`) is unavailable: too large: over 4 MiB (4501538 bytes) — a declared schema source is read only up to 4 MiB (4194304 bytes)"),
        "{stderr}"
    );
}

#[test]
fn ambiguous_claim_never_validates_parse_only() {
    // r51 #3: a file two live manifests claim validates under NO
    // binding. Pre-fix a claimed file that did not compose (no `uses`)
    // fell to "the flags decide" — `shared/nouses.flow.nml: ok`, exit 0
    // — while `nml binding` called it AMBIGUOUS. The ambiguity is now an
    // error finding every verb counts: `check` and `validate` exit 1
    // before the file is read, `fix` refuses it, `binding` reports it
    // under `notes` in the words the row uses.
    let dir = workspace_copy("ambiguous");
    std::fs::write(dir.join("shared/nouses.flow.nml"), "thing t:\n    a = 1\n").unwrap();
    let file = dir.join("shared/nouses.flow.nml");
    let summary = "2 manifests claim this file: demo.package.nml (shared, files[0] = \
                   \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \
                   \"shared/**/*.flow.nml\")";
    let denied = format!(
        "shared/nouses.flow.nml: error[NML2087]: {summary} — an ambiguously-claimed file is denied"
    );
    let (code, stdout, stderr) = check_under(&dir, "shared/nouses.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(!stdout.contains("ok"), "{stdout}");
    assert!(stderr.contains(&denied), "{stderr}");
    assert!(stderr.trim_end().ends_with("error: 1 error(s)"), "{stderr}");
    let (code, stdout, stderr) = run(&[
        "validate",
        "--root",
        dir.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains(&denied), "{stderr}");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 1,
        "a refused path could not be fixed: {stdout}{stderr}"
    );
    assert!(stderr.contains(&denied), "{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    assert!(
        stdout.contains(
            "0 edit(s) would apply across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable"
        ),
        "{stdout}"
    );
    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains(&format!("binding   AMBIGUOUS — {summary}\n")),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("notes     {denied}")), "{stdout}");
}

#[test]
fn target_spelled_through_the_roots_parent_is_inside_the_root() {
    // r51 #4: from the root's own directory, `../<root>/…` names a file
    // INSIDE the root — the `..` pops the root itself and the next
    // component re-enters it through the canonical parent. Pre-fix the
    // fold stopped AT the root before consuming the `..`, handed it to
    // the policy walk, and the walk called it an escape: "is outside the
    // workspace root", exit 1, with and without `--root`.
    let root = fixture("workspace");
    let name = root.file_name().unwrap().to_str().unwrap();
    let target = format!("../{name}/tenants/cu/plain.flow.nml");
    let (code, stdout, stderr) = run_in(&root, &["check", "--root", ".", &target]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    let (code, stdout, stderr) = run_in(&root, &["check", &target]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stderr.contains("outside the workspace root"), "{stderr}");
    let (code, stdout, stderr) = run_in(&root, &["binding", "--root", ".", &target]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("file      tenants/cu/plain.flow.nml\n"),
        "{stdout}"
    );
    // A genuine escape through the parent stays one: the root's sibling
    // — the invocation's mistake, exit 2 (r85 D3).
    let (code, _, stderr) = run_in(
        &root,
        &["check", "--root", ".", &format!("../{name}-open/x.nml")],
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("is outside the workspace root"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn fix_renders_a_closed_binding_rejection_like_check() {
    // NML2083 through `nml fix` carries severity, code and the explain
    // hint, exactly as `check` prints it (never `<path>: closed binding
    // rejects …`, bare), and the refused path could not be fixed: exit
    // 1, as an absent path.
    let dir = scratch_dir("fix-2083");
    copy_tree(&fixture("workspace-link-a"), &dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let file = dir.join("tenants/cu/lib/base.flow.nml");
    let (code, stdout, stderr) = run(&["fix", "--dry-run", file.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "tenants/cu/lib/base.flow.nml: error[NML2083]: closed binding rejects \
             `tenants/cu/lib/base.flow.nml`: path component `lib` is a symlink"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML2083"),
        "{stderr}"
    );
    assert!(
        stdout.contains(
            "0 edit(s) would apply across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable"
        ),
        "{stdout}"
    );
    assert!(!stderr.contains("vendor"), "{stderr}");
}

#[test]
fn run_bounded_drains_a_chatty_child() {
    // r52 #10: `run_bounded` polled `try_wait` without reading the pipes,
    // so a child writing more than the pipe holds (64 KiB on the usual
    // platforms) blocked on its write and read as a hang — a green
    // harness only because every bounded drive so far printed a few
    // lines. `nml parse` over 300 declarations prints ~400 KiB of JSON:
    // pre-fix this panicked "hung for 5s"; now it returns the output.
    let dir = scratch_dir("chatty");
    let file = dir.join("chatty.nml");
    let text: String = (0..300)
        .map(|i| format!("thing t{i}:\n    a = {i}\n"))
        .collect();
    std::fs::write(&file, text).unwrap();
    let (code, stdout, stderr) = run_bounded(&["parse", file.to_str().unwrap()], 5);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.len() > 256 * 1024, "{} bytes", stdout.len());
}

/// `nml fix` (dry run and real run) over every spelling the r52 and r54
/// certifiers split on the link fixture at `root` — the file through the
/// directory link, the directory link itself, a leaf link, the link
/// continued past by a separator, a `.`, a `..` and a doubled `..`,
/// (r56) a real directory inside the link's target, one level and two,
/// and (r58) the real directory that CONTAINS the links — with and
/// without `--root`, normalized: one entry per invocation.
#[cfg(unix)]
fn fix_link_outputs(root: &Path, variant: &str) -> Vec<String> {
    let canonical = std::fs::canonicalize(root).unwrap().display().to_string();
    let mut outputs = Vec::new();
    for spelling in [
        "tenants/cu/lib/base.flow.nml",
        "tenants/cu/lib",
        "tenants/cu/leaf.flow.nml",
        "tenants/cu/lib/",
        "tenants/cu/lib/.",
        "tenants/cu/lib/..",
        "tenants/cu/lib/../..",
        "tenants/cu/lib/sub",
        "tenants/cu/lib/sub/deeper",
        "tenants/cu",
    ] {
        let file = root.join(spelling);
        for (dry_run, with_root) in [(true, false), (true, true), (false, false), (false, true)] {
            let mut args = vec!["fix"];
            if dry_run {
                args.push("--dry-run");
            }
            if with_root {
                args.extend(["--root", root.to_str().unwrap()]);
            }
            args.push(file.to_str().unwrap());
            let (code, stdout, stderr) = run(&args);
            outputs.push(
                format!(
                    "{spelling} dry_run={dry_run} root={with_root}\nexit {code}\n{stdout}{stderr}"
                )
                .replace(&canonical, "<root>")
                .replace(root.to_str().unwrap(), "<root>")
                .replace(variant, "<v>"),
            );
        }
    }
    outputs
}

#[cfg(unix)]
#[test]
fn fix_argv_classification_is_not_an_existence_oracle() {
    // r52 MAJOR #1: `fix` classified its arguments with a FOLLOWING stat
    // (`is_dir()` / `is_file()`) before the universe walk, so under a
    // closed binding an existing author link drew NML2083 while a
    // dangling one drew "error: no such file or directory", exit 1 — the
    // oracle E26 closed for `check`, `validate` and `binding`, open
    // through the verb CI feeds changed-file lists to. Classification is
    // lstat now: a link (to a file, to a directory, dangling) or an absent
    // path is a file candidate the walk judges. Each link fixture is
    // copied under a fresh `.git` (the fenced regime, deterministic) with
    // a leaf link added; a and b differ only in whether the links' targets
    // exist. Byte-identical across a/b for every spelling, exit 0, NML2083
    // naming the link component — never the target, never "no such file".
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        let dir = scratch_dir(&format!("fix-oracle-{v}"));
        copy_tree(&fixture(&format!("workspace-link-{v}")), &dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::os::unix::fs::symlink(
            "../../vendor/base.flow.nml",
            dir.join("tenants/cu/leaf.flow.nml"),
        )
        .unwrap();
        if v == "a" {
            // r56 (E32): fixture `a` carries a REAL directory inside the
            // link's target, `vendor/sub/` (one file, one deeper). `lstat`
            // refuses to follow only the leaf, so `lib/sub` classified as
            // a directory and walked the target — its `.nml` names listed,
            // `0 of 2 file(s)` — while the dangling half drew one NML2083
            // line naming `lib/sub`.
            assert!(dir.join("vendor/sub/deep.flow.nml").is_file());
            assert!(dir.join("vendor/sub/deeper/x.flow.nml").is_file());
        }
        let mut outs = fix_link_outputs(&dir, &format!("fix-oracle-{v}"));
        // The same spelling relative to the root (the CI shape).
        let (code, stdout, stderr) = run_in(&dir, &["fix", "--dry-run", "tenants/cu/lib/sub"]);
        // The disclosure note names the derived root (the scratch
        // directory sits under the checkout's own `.git`): normalized
        // like every other row.
        outs.push(
            format!("tenants/cu/lib/sub relative\nexit {code}\n{stdout}{stderr}")
                .replace(dir.to_str().unwrap(), "<root>")
                .replace(&format!("fix-oracle-{v}"), "<v>"),
        );
        // E35 (r60 finding 3, E34's noted row): a `..` that pops above
        // the root AFTER a component under it was walked is `Escapes` —
        // the kernel pops lexically once inside — so `vendor/../../<dir>/
        // tenants/cu` is ONE "outside" line, exactly as `check` says,
        // never a physical re-location that walks `tenants/cu` and lists
        // it. Relative and absolute under `--root` (the root derived from
        // such a spelling is E28's `target_dir`'s business — on `b` the
        // `vendor` it passes through does not exist), byte-identical a/b.
        let base = dir.file_name().unwrap().to_str().unwrap().to_string();
        let popped = format!("vendor/../../{base}/tenants/cu");
        let canonical = std::fs::canonicalize(&*dir).unwrap().display().to_string();
        for (label, args) in [
            (
                "popped relative --root",
                vec![
                    "fix",
                    "--dry-run",
                    "--root",
                    dir.to_str().unwrap(),
                    popped.as_str(),
                ],
            ),
            (
                "popped absolute --root",
                vec![
                    "fix",
                    "--dry-run",
                    "--root",
                    dir.to_str().unwrap(),
                    &format!("{}/{popped}", dir.display()),
                ],
            ),
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (code, stdout, stderr) = run_in(&dir, &args);
            outs.push(
                format!("{label}\nexit {code}\n{stdout}{stderr}")
                    .replace(&canonical, "<root>")
                    .replace(dir.to_str().unwrap(), "<root>")
                    .replace(&base, "<root>")
                    .replace(&format!("fix-oracle-{v}"), "<v>"),
            );
        }
        outputs.push(outs);
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0].len(), 43);
    for out in &outputs[0] {
        if out.starts_with("popped ") {
            // The invocation's mistake (r85 D3): exit 2, and nothing
            // ran — no fixer tally at all.
            assert!(out.contains("\nexit 2\n"), "{out}");
            assert_eq!(
                out.matches("is outside the workspace root").count(),
                1,
                "{out}"
            );
            assert!(!out.contains("file(s)"), "nothing ran: {out}");
            for leaked in ["leaf.flow", "lib", "of 2 file(s)", "no .nml files found"] {
                assert!(!out.contains(leaked), "walked {leaked}: {out}");
            }
            continue;
        }
        // r58 MINOR #3 (E33): the walk's follow-nothing rule. `tenants/cu`
        // is a real directory holding the author link `lib` and the leaf
        // link only; the walk classifies each entry by `file_type()`
        // (lstat), so both are skipped and nothing is collected — "no
        // .nml files found", exit 1, byte-identical dangling or existing.
        // A FOLLOWING stat would recurse `lib → vendor` on `a` and push
        // `leaf.flow.nml` through the link — its `.nml` names listed —
        // while `b` (dangling) still found nothing.
        if out.starts_with("tenants/cu dry_run=") {
            assert!(out.contains("\nexit 1\n"), "{out}");
            assert!(out.contains("no .nml files found"), "{out}");
            assert!(!out.contains("file(s)"), "walked: {out}");
            for leaked in ["vendor", "base.flow", "deep.flow", "x.flow", "leaf.flow"] {
                assert!(!out.contains(leaked), "listed {leaked}: {out}");
            }
            continue;
        }
        // r54 MAJOR #1: one lstat of the spelling AS TYPED follows the
        // link when the spelling continues past it. Pre-fix `lib/` and
        // `lib/.` walked the link's target (its `.nml` names listed);
        // `lib/..` named the TARGET's parent — the root: `0 of 3
        // file(s)`, `core.model.nml`, `demo.package.nml` and
        // `vendor/base.flow.nml` listed — and `lib/../..` the root's
        // parent, while the dangling half exited 1. Now every one is a
        // file candidate: the separator and `.` spellings are judged
        // like `lib` (below); a `..` leaf is refused as `check` refuses
        // it, exit 1, nothing walked, nothing listed.
        if out.starts_with("tenants/cu/lib/..") {
            assert!(out.contains("\nexit 1\n"), "{out}");
            // E35: a failed argument no longer aborts the run — the
            // summary prints (`0 of 1`: nothing was walked).
            assert!(out.contains("0 of 1 file(s)"), "walked: {out}");
            for leaked in ["vendor", "core.model", "demo.package", "of 2", "of 3"] {
                assert!(!out.contains(leaked), "listed {leaked}: {out}");
            }
            continue;
        }
        assert!(
            out.contains("\nexit 1\n"),
            "a refused path could not be fixed: {out}"
        );
        assert!(out.contains("error[NML2083]"), "{out}");
        assert!(out.contains("1 diagnostic(s) not auto-fixable"), "{out}");
        assert!(out.contains("1 path(s) could not be fixed"), "{out}");
        assert!(!out.contains("vendor"), "never the target: {out}");
        for leaked in ["deep.flow.nml", "x.flow.nml", "of 2 file(s)"] {
            assert!(!out.contains(leaked), "walked into the target: {out}");
        }
        assert!(!out.contains("no such file"), "{out}");
        let component = if out.starts_with("tenants/cu/leaf") {
            "leaf.flow.nml"
        } else {
            "lib"
        };
        assert!(
            out.contains(&format!("path component `{component}` is a symlink")),
            "{out}"
        );
    }
}

#[cfg(unix)]
#[test]
fn fix_follows_an_operator_link_above_the_root() {
    // r56 (E32): the classification locates the root by the operator's
    // realpath, so a link ABOVE the root (an alias directory; `/tmp` →
    // `/private/tmp`) and a `..` through it are followed exactly as
    // `check` follows them — the E31 residual (`nml fix /tmp/../proj/dir`
    // → "Is a directory") is withdrawn; from the root down nothing is
    // followed. Fixture `a` under the alias: `vendor/` holds three files.
    let dir = scratch_dir("fix-oplink");
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    copy_tree(&fixture("workspace-link-a"), &dir.join("proj"));
    std::fs::create_dir_all(dir.join("proj/.git")).unwrap();
    std::os::unix::fs::symlink("proj", dir.join("alias")).unwrap();
    let root = dir.join("proj");
    let mut rows: Vec<(String, std::path::PathBuf)> = vec![];
    for spelling in [
        "alias/vendor",
        "alias/../alias/vendor",
        "proj/../alias/vendor",
    ] {
        rows.push((root.to_str().unwrap().to_string(), dir.join(spelling)));
    }
    // `--root` given through the alias too.
    rows.push((
        dir.join("alias").to_str().unwrap().to_string(),
        dir.join("alias/vendor"),
    ));
    // The scratch tree spelled through an operator link above it (`/tmp`
    // → `/private/tmp` on macOS: `$CARGO_TARGET_TMPDIR` sits under it
    // here); where `/tmp` is real the alias rows above carry the rule.
    let tmp_real = std::fs::canonicalize("/tmp").unwrap_or_default();
    if tmp_real != Path::new("/tmp") {
        if let Ok(rest) = dir.strip_prefix(&tmp_real) {
            let spelled = Path::new("/tmp").join(rest);
            for spelling in ["proj/../proj/vendor", "proj/vendor", "alias/vendor"] {
                rows.push((root.to_str().unwrap().to_string(), spelled.join(spelling)));
            }
            rows.push((
                spelled.join("proj").to_str().unwrap().to_string(),
                spelled.join("proj/../proj/vendor"),
            ));
        }
    }
    assert!(rows.len() >= 4);
    for (root_arg, target) in &rows {
        let (code, stdout, stderr) = run(&[
            "fix",
            "--dry-run",
            "--root",
            root_arg,
            target.to_str().unwrap(),
        ]);
        let label = format!("--root {root_arg} {}", target.display());
        assert_eq!(code, 0, "{label}: {stdout}{stderr}");
        assert!(stdout.contains("of 3 file(s)"), "{label}: {stdout}{stderr}");
        assert!(!stderr.contains("Is a directory"), "{label}: {stderr}");
    }
    // r58 NIT #4 (E33): a `..` that pops the root from depth 1 returns
    // to locating, and locating re-enters the root by canonical depth —
    // `vendor/../vendor/sub` (E32's "unchanged" row) walks `sub`: `of 2`.
    // A link AT the root's child reached the same way (`rootlink →
    // vendor`, `vendor/../rootlink/sub`) is met in `lstat` mode, never
    // canonicalized: a file candidate, NML2083 naming `rootlink`, `of 1`,
    // nothing under `vendor/sub` listed. A depth counter that returned to
    // locating one level early (`depth <= 1` → locating) canonicalized
    // `rootlink` and walked `sub` — `of 2`, `deep.flow.nml` listed.
    std::os::unix::fs::symlink("vendor", root.join("rootlink")).unwrap();
    let root_arg = root.to_str().unwrap();
    let popped = dir.join("proj/vendor/../vendor/sub");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        root_arg,
        popped.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("of 2 file(s)"), "{stdout}{stderr}");
    let through_link = dir.join("proj/vendor/../rootlink/sub");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        root_arg,
        through_link.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 1,
        "a refused path could not be fixed: {stdout}{stderr}"
    );
    assert!(stdout.contains("0 of 1 file(s)"), "{stdout}{stderr}");
    assert!(
        stderr.contains("path component `rootlink` is a symlink"),
        "{stderr}"
    );
    for leaked in ["of 2 file(s)", "deep.flow.nml", "x.flow.nml"] {
        assert!(
            !stdout.contains(leaked),
            "walked through rootlink: {stdout}"
        );
        assert!(
            !stderr.contains(leaked),
            "walked through rootlink: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn fix_never_walks_through_an_author_link_behind_an_operator_alias() {
    // r58 MAJOR #1 (E33): the classifier located the root only where a
    // prefix's canonical path EQUALLED the root, while the kernel stops
    // where it is at or INSIDE the root — an operator link into the
    // root's interior included (E28). Through such an alias (`alias-cu
    // → proj/tenants/cu`, the operator's, outside the root) no prefix
    // equalled the root until the AUTHOR's link did: `tenants/cu/toroot
    // → ../..` canonicalizes to the root, so `alias-cu/toroot/vendor`
    // located the root AT the author link and walked its target — `0 of
    // 3 file(s)`, three NML2083 lines naming `base.flow.nml` and the
    // rest — while the dangling half (`toroot → ../../nope`) drew one
    // line naming `toroot`. The sharp oracle: an author link THROUGH an
    // outside directory back to the root (`probe → ../../../probe/../
    // proj` against `…/probe-nope/../proj`) — the halves differ only in
    // whether a directory OUTSIDE the root exists. Now the root is
    // located where a prefix is at or inside it, with the depth set to
    // the canonical components already inside, so every author link is
    // met in `lstat` mode whatever it points at: byte-identical across
    // a/b, NML2083 naming the link, `0 of 1`, never a target's file
    // name; `check` and `binding` were identical at the link all along.
    let mut outputs = Vec::new();
    for v in ["a", "b"] {
        let tag = format!("fix-alias-{v}");
        let dir = scratch_dir(&tag);
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        copy_tree(&fixture(&format!("workspace-link-{v}")), &proj);
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        // The author's links under `tenants/cu`: to the root itself, and
        // to the root THROUGH an outside directory (`probe/` exists
        // beside `proj` in `a` only). In `b` both dangle.
        let (toroot, probe) = if v == "a" {
            std::fs::create_dir_all(dir.join("probe")).unwrap();
            ("../..", "../../../probe/../proj")
        } else {
            ("../../nope", "../../../probe-nope/../proj")
        };
        std::os::unix::fs::symlink(toroot, proj.join("tenants/cu/toroot")).unwrap();
        std::os::unix::fs::symlink(probe, proj.join("tenants/cu/probe")).unwrap();
        // r60 (E34): an author link BESIDE `cu`, reached by popping out
        // of the alias — the depth VALUE set at an alias into the
        // interior is what keeps the `..` from returning to locating
        // (a depth counted from the spelling was `0` at `alias-cu`, and
        // locating canonicalized `alias-cu/../lib3` through the link).
        let lib3 = if v == "a" { "../vendor" } else { "../nope" };
        std::os::unix::fs::symlink(lib3, proj.join("tenants/lib3")).unwrap();
        // The operator's aliases, OUTSIDE the root, into its interior.
        std::os::unix::fs::symlink("proj/tenants/cu", dir.join("alias-cu")).unwrap();
        std::os::unix::fs::symlink("proj/vendor", dir.join("alias-v")).unwrap();
        let root_arg = proj.to_str().unwrap();
        let canonical = std::fs::canonicalize(&proj).unwrap().display().to_string();
        let mut outs = Vec::new();
        for spelling in [
            "alias-cu/toroot/vendor",
            "alias-cu/toroot/tenants/cu",
            "alias-cu/probe/vendor",
            "alias-cu/probe/tenants/cu",
            "alias-cu/../lib3/sub",
            "alias-cu/../lib3",
            "alias-cu/../../tenants/lib3/sub",
        ] {
            let target = dir.join(spelling);
            for dry_run in [true, false] {
                let mut args = vec!["fix"];
                if dry_run {
                    args.push("--dry-run");
                }
                args.extend(["--root", root_arg, target.to_str().unwrap()]);
                let (code, stdout, stderr) = run(&args);
                outs.push(
                    format!("{spelling} dry_run={dry_run}\nexit {code}\n{stdout}{stderr}")
                        .replace(&canonical, "<root>")
                        .replace(dir.to_str().unwrap(), "<dir>")
                        .replace(&tag, "<v>"),
                );
            }
        }
        outputs.push(outs);
        if v == "a" {
            // The alias itself is operator territory (E32): `alias-v/sub`
            // is a real directory two canonical components inside the
            // root, walked — `of 2`, exit 0. Pre-fix no prefix equalled
            // the root: a file candidate, "Is a directory", exit 1.
            let target = dir.join("alias-v/sub");
            let (code, stdout, stderr) = run(&[
                "fix",
                "--dry-run",
                "--root",
                root_arg,
                target.to_str().unwrap(),
            ]);
            assert_eq!(code, 0, "{stdout}{stderr}");
            assert!(stdout.contains("of 2 file(s)"), "{stdout}{stderr}");
            assert!(!stderr.contains("Is a directory"), "{stderr}");
        }
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0].len(), 14);
    for out in &outputs[0] {
        assert!(
            out.contains("\nexit 1\n"),
            "a refused path could not be fixed: {out}"
        );
        assert!(out.contains("error[NML2083]"), "{out}");
        assert!(out.contains("0 of 1 file(s)"), "{out}");
        assert!(out.contains("1 diagnostic(s) not auto-fixable"), "{out}");
        let link = if out.starts_with("alias-cu/toroot") {
            "toroot"
        } else if out.starts_with("alias-cu/probe") {
            "probe"
        } else {
            "lib3"
        };
        assert!(
            out.contains(&format!("path component `{link}` is a symlink")),
            "{out}"
        );
        for leaked in [
            "base.flow.nml",
            "core.model",
            "demo.package",
            "deep.flow",
            "x.flow",
            "of 2 file(s)",
            "of 3 file(s)",
            "no .nml files found",
            "no such file",
        ] {
            assert!(!out.contains(leaked), "walked through {link}: {out}");
        }
    }
}

#[test]
fn fix_refuses_an_outside_directory_argument_without_walking_it() {
    // r56 (E32, delta c): a directory argument OUTSIDE the root is one
    // error naming the directory. Pre-fix the directory was walked first
    // and the error named the first `.nml` file found inside it — a
    // listing of a tree the universe does not own.
    let dir = scratch_dir("fix-outside-dir");
    std::fs::create_dir_all(dir.join("proj/.git")).unwrap();
    copy_tree(&fixture("workspace"), &dir.join("proj"));
    let _ = std::fs::remove_dir_all(dir.join("proj/expected"));
    std::fs::create_dir_all(dir.join("outside/d")).unwrap();
    std::fs::write(dir.join("outside/d/o.flow.nml"), "thing o:\n    a = 1\n").unwrap();
    let outside = dir.join("outside/d");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.join("proj").to_str().unwrap(),
        outside.to_str().unwrap(),
    ]);
    // The invocation's mistake (r85 D3): exit 2, refused before any
    // walk — no fixer tally at all is the proof nothing was walked.
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(stderr.contains("is outside the workspace root"), "{stderr}");
    assert!(stderr.contains(outside.to_str().unwrap()), "{stderr}");
    assert!(
        !stderr.contains("o.flow.nml"),
        "walked the outside tree: {stderr}"
    );
    assert!(!stdout.contains("file(s)"), "nothing ran: {stdout}");
    // r60 (E34): a sibling whose NAME has the root's as a string prefix
    // (`projx` beside `proj`) is outside all the same — the locator
    // compares canonical paths component-wise, never as strings. A
    // string-prefix test counted `projx/vendor` as inside, walked it,
    // and drew one "outside" line PER FILE naming each. Exactly one
    // line, naming the directory, no file named.
    std::fs::create_dir_all(dir.join("projx/vendor")).unwrap();
    std::fs::write(
        dir.join("projx/vendor/px.flow.nml"),
        "thing px:\n    a = 1\n",
    )
    .unwrap();
    let sibling = dir.join("projx/vendor");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.join("proj").to_str().unwrap(),
        sibling.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert_eq!(
        stderr.matches("is outside the workspace root").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains(sibling.to_str().unwrap()), "{stderr}");
    assert!(
        !stderr.contains("px.flow.nml"),
        "walked the sibling tree: {stderr}"
    );
    assert!(!stdout.contains("file(s)"), "nothing ran: {stdout}");
}

#[test]
fn fix_reports_a_bad_root_before_an_empty_walk() {
    // r56 (E32, delta d): the workspace opens BEFORE the arguments are
    // classified, so a `--root` that is not a directory is the error —
    // pre-fix an argument tree with no `.nml` file drew "no .nml files
    // found" first and the bad root was never mentioned.
    let dir = scratch_dir("fix-badroot");
    std::fs::create_dir_all(dir.join("nothing")).unwrap();
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.join("nonexistent").to_str().unwrap(),
        dir.join("nothing").to_str().unwrap(),
    ]);
    assert_eq!(
        code, 2,
        "a `--root` that is no directory is a usage error: {stdout}{stderr}"
    );
    assert!(stderr.contains("--root"), "{stderr}");
    assert!(stderr.contains("not a directory"), "{stderr}");
    assert!(!stderr.contains("no .nml files found"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn fix_walk_skips_a_fifo_without_opening_it() {
    // r56 (E32, NIT #4): a FIFO named `*.nml` INSIDE a walked directory is
    // skipped like a link — the walk pushes regular files only. Pre-fix
    // the walk pushed every non-directory entry and the read blocked
    // forever (a tenant DoS of `nml fix tenants/cu` in the operator's
    // CI). A FIFO named on the command line stays the operator's, as
    // `check` treats it. Watchdog-bounded: a hang is a failure.
    let dir = workspace_copy("fix-fifo");
    let pipe = dir.join("tenants/cu/pipe.flow.nml");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("mkfifo runs");
    assert!(status.success());
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_bounded(
        &[
            "fix",
            "--dry-run",
            "--root",
            dir.to_str().unwrap(),
            dir.join("tenants/cu").to_str().unwrap(),
        ],
        20,
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("of 4 file(s)"), "{stdout}{stderr}");
    assert!(!stdout.contains("pipe.flow.nml"), "{stdout}");
    assert!(!stderr.contains("pipe.flow.nml"), "{stderr}");
}

#[test]
fn fix_absent_path_fails_at_the_read_like_check() {
    // r52 MAJOR #1, the absent half: an argument that does not exist is
    // no longer refused by an early stat ("error: no such file or
    // directory: <arg>"); it is a file candidate, resolved under the
    // universe like any other, and fails where `check` fails — at the
    // read, in the same words — so absence is disclosed after
    // resolution, never before it. A bound key (the tenant glob claims
    // it), so both verbs reach the read.
    let dir = workspace_copy("fix-absent");
    let file = dir.join("tenants/cu/nope.flow.nml");
    let (fix_code, fix_out, fix_err) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    let (check_code, _, check_err) = check_under(&dir, "tenants/cu/nope.flow.nml");
    assert_eq!(
        (fix_code, check_code),
        (1, 1),
        "{fix_out}{fix_err}{check_err}"
    );
    // r69a (r68 UX F5): the kernel's walk found no leaf at the minted
    // key, so both verbs say so — no OS error, no "failed to read".
    let absent = format!("error: {}: no such file or directory", file.display());
    assert!(fix_err.contains(&absent), "{fix_err}");
    assert!(check_err.contains(&absent), "{check_err}");
    assert!(!fix_err.contains("failed to read"), "{fix_err}");
    assert!(!check_err.contains("os error"), "{check_err}");
}

#[cfg(unix)]
#[test]
fn fix_symlinked_directory_argument_is_never_walked() {
    // r52 MAJOR #1, the open-universe corner: a directory argument that
    // is a symlink used to be walked THROUGH the link (`is_dir()` follows
    // it) — the one place the fixer's follow-nothing rule did not hold.
    // Under lstat classification it is a file candidate: with no manifest
    // anywhere the walk resolves it, and the open refuses the resolved
    // leaf — a directory — in the walk's own words, exit 1, without
    // touching what the link points at. Pre-fix: exit 0, `0 edit(s)
    // would apply across 0 of 1 file(s)` for `link/x.nml`; then the OS's
    // `Is a directory (os error 21)`.
    let dir = scratch_dir("fix-dirlink");
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::fs::write(dir.join("real/x.nml"), "thing t:\n    a = 1\n").unwrap();
    std::os::unix::fs::symlink("real", dir.join("link")).unwrap();
    let link = dir.join("link");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        link.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "error: `{}` is a directory the universe walk did not enter",
            link.display()
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("os error"), "{stderr}");
    assert!(
        stdout.contains("0 of 1 file(s)"),
        "walked through the link: {stdout}"
    );
    assert!(
        !stdout.contains("x.nml"),
        "walked through the link: {stdout}"
    );
}

/// E35 (r60 A8–A10): an operator alias TO the root — bare, with a
/// separator, with `/.` — names the root, and `fix` walks it exactly as
/// `fix <root>` does (byte-identical). The deleted classifier refused
/// these as "not a relative path", exit 1.
#[cfg(unix)]
#[test]
fn fix_walks_the_root_through_an_operator_alias_to_it() {
    let dir = scratch_dir("fix-alias-root");
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    copy_tree(&fixture("workspace-link-a"), &dir.join("proj"));
    std::fs::create_dir_all(dir.join("proj/.git")).unwrap();
    std::os::unix::fs::symlink("proj", dir.join("alias-root")).unwrap();
    let root = dir.join("proj");
    let root_arg = root.to_str().unwrap();
    let reference = run(&["fix", "--dry-run", "--root", root_arg, root_arg]);
    assert_eq!(reference.0, 0, "{reference:?}");
    assert!(reference.1.contains("file(s)"), "{reference:?}");
    for spelling in ["alias-root", "alias-root/", "alias-root/."] {
        let target = dir.join(spelling);
        let out = run(&[
            "fix",
            "--dry-run",
            "--root",
            root_arg,
            target.to_str().unwrap(),
        ]);
        assert_eq!(out, reference, "{spelling}");
        assert!(
            !out.2.contains("not a relative path"),
            "{spelling}: {}",
            out.2
        );
    }
}

/// E35 (r60 A2): an operator alias AT an author link (`alias-lib →
/// proj/tenants/cu/lib`, the author's `lib → ../../vendor`) is the
/// OPERATOR's spelling and resolves fully (E28: an operator link into
/// the root's interior, its target canonicalized): where `vendor` exists
/// it walks — `of 3` — and where the author link dangles the alias
/// itself resolves nowhere, outside the root. The halves differ by
/// design (kernel row 10, `check` says the same: `alias-lib/sub` walked
/// already); the deleted classifier said "Is a directory", exit 1, on
/// the existing half.
#[cfg(unix)]
#[test]
fn fix_walks_an_operator_alias_at_an_author_link() {
    for v in ["a", "b"] {
        let dir = scratch_dir(&format!("fix-alias-lib-{v}"));
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        copy_tree(&fixture(&format!("workspace-link-{v}")), &proj);
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::os::unix::fs::symlink("proj/tenants/cu/lib", dir.join("alias-lib")).unwrap();
        let target = dir.join("alias-lib");
        let (code, stdout, stderr) = run(&[
            "fix",
            "--dry-run",
            "--root",
            proj.to_str().unwrap(),
            target.to_str().unwrap(),
        ]);
        assert!(!stderr.contains("Is a directory"), "{stderr}");
        if v == "a" {
            assert_eq!(code, 0, "{stdout}{stderr}");
            assert!(stdout.contains("of 3 file(s)"), "{stdout}{stderr}");
            // `check` agrees: the same file under the same alias is read.
            let (c, _, e) = run(&[
                "check",
                "--root",
                proj.to_str().unwrap(),
                dir.join("alias-lib/base.flow.nml").to_str().unwrap(),
            ]);
            assert_eq!(c, 0, "{e}");
        } else {
            // Outside the root: the invocation's mistake, exit 2, nothing
            // ran (r85 D3).
            assert_eq!(code, 2, "{stdout}{stderr}");
            assert!(stderr.contains("is outside the workspace root"), "{stderr}");
            assert!(!stdout.contains("file(s)"), "nothing ran: {stdout}");
        }
    }
}

/// Restores a locked directory's mode on every path, so the scratch
/// directory can remove itself even after a failed assertion.
#[cfg(unix)]
struct Unlock(std::path::PathBuf);

#[cfg(unix)]
impl Drop for Unlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// E35 (r60 P2/P4, arch finding 1b): an EACCES prefix inside the root
/// closes the universe (A16, unreadable directory) and `fix` says what
/// `check` says — the universe error, with the CLI's `--root` advice —
/// BEFORE any argument is expanded, never an OS error from a walk it
/// should not have started, nothing listed. r75: that is the ROOT
/// unit's story (`vendor/` is content no glob reaches). An unlistable
/// directory INSIDE a budget unit is that unit's alone now
/// (`Truncation::Unreadable` is unit-scoped, r74-kernel F5): the unit's
/// NML2089 on every key under it, no `--root` advice, the sibling
/// tenant untouched — and `fix` still never walks a directory argument
/// under the spent unit (E35's property, kept for units).
#[cfg(unix)]
#[test]
fn fix_reports_an_unreadable_prefix_as_the_universe_error_like_check() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("fix-eacces");
    std::fs::create_dir_all(dir.join("vendor/locked/sub")).unwrap();
    std::fs::write(
        dir.join("vendor/locked/sub/l.flow.nml"),
        "thing l:\n    v = \"l\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.join("vendor/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(dir.join("vendor/locked"));
    if std::fs::read_dir(dir.join("vendor/locked")).is_ok() {
        // Running as root: the directory is readable regardless and the
        // row has nothing to prove here.
        return;
    }
    let (check_code, _, check_err) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(check_code, 1, "{check_err}");
    let stopped =
        "the walk stopped at `vendor/locked` (unreadable: permission denied on a path component)";
    assert!(check_err.contains(stopped), "{check_err}");
    assert!(
        check_err.contains("pass --root to a smaller tree that still holds your manifests"),
        "{check_err}"
    );
    for target in ["vendor/locked", "vendor/locked/../base.flow.nml", "vendor"] {
        let (code, stdout, stderr) = run(&[
            "fix",
            "--dry-run",
            "--root",
            dir.to_str().unwrap(),
            dir.join(target).to_str().unwrap(),
        ]);
        assert_eq!(code, 1, "{target}: {stdout}{stderr}");
        assert!(stderr.contains(stopped), "{target}: {stderr}");
        assert!(
            stderr.contains("pass --root to a smaller tree that still holds your manifests"),
            "{target}: {stderr}"
        );
        assert!(
            !stderr.contains("Permission denied (os error"),
            "{target}: {stderr}"
        );
        assert!(!stdout.contains("file(s)"), "{target}: listed: {stdout}");
        assert!(!stderr.contains("l.flow.nml"), "{target}: listed: {stderr}");
    }
}

/// r75 (r74-kernel F5): an unlistable directory INSIDE a budget unit
/// denies that unit alone. `check` on the tenant's file exits 1 with
/// the unit's NML2089 on the key — naming the unlistable directory,
/// offering no `--root` (rooting inside the unit re-opens the
/// universe) — the sibling tenant validates in the same universe, and
/// `fix` on a directory argument under the unit never walks it (no OS
/// error, nothing listed): the denial prints, `--dry-run` keeps the
/// fixer's exit 0, `--check` gates on it.
#[cfg(unix)]
#[test]
fn an_unlistable_directory_inside_a_unit_denies_the_unit_alone_in_every_verb() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("unit-eacces");
    std::fs::create_dir_all(dir.join("tenants/du")).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        dir.join("tenants/du/plain.flow.nml"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("tenants/cu/locked/sub")).unwrap();
    std::fs::write(
        dir.join("tenants/cu/locked/sub/l.flow.nml"),
        "thing l:\n    v = \"l\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.join("tenants/cu/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(dir.join("tenants/cu/locked"));
    if std::fs::read_dir(dir.join("tenants/cu/locked")).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.to_str().unwrap();
    let denial = "discovery under `tenants/cu` was cut short: the walk stopped at \
                  `tenants/cu/locked` (unreadable: permission denied on a path component)";
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "tenants/cu/plain.flow.nml: error[NML2089]: {denial}"
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("--root"), "{stderr}");
    assert!(!stderr.contains("cannot enumerate manifests"), "{stderr}");
    let (code, stdout, stderr) = check_under(&dir, "tenants/du/plain.flow.nml");
    assert_eq!(code, 0, "the sibling is untouched: {stdout}{stderr}");
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        root,
        dir.join("tenants/du/plain.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let last = rows.last().unwrap();
    assert_eq!(last["closure"], "complete", "{last}");
    assert_eq!(
        last["truncatedUnits"],
        serde_json::json!([{"unit": "tenants/cu", "stop": "tenants/cu/locked", "why": "unreadable"}]),
        "{last}"
    );
    for target in [
        "tenants/cu/locked",
        "tenants/cu/locked/../plain.flow.nml",
        "tenants/cu",
    ] {
        let (code, stdout, stderr) = run(&[
            "fix",
            "--dry-run",
            "--root",
            root,
            dir.join(target).to_str().unwrap(),
        ]);
        assert_eq!(
            code, 1,
            "{target}: a denied path could not be fixed: {stdout}{stderr}"
        );
        assert!(stderr.contains(denial), "{target}: {stderr}");
        assert!(!stderr.contains("--root"), "{target}: {stderr}");
        assert!(
            !stderr.contains("Permission denied (os error"),
            "{target}: never walked: {stderr}"
        );
        assert!(!stderr.contains("l.flow.nml"), "{target}: listed: {stderr}");
        assert!(
            stdout.contains("0 edit(s) would apply across 0 of 1 file(s)"),
            "{target}: {stdout}"
        );
        let (code, _, stderr) = run(&[
            "fix",
            "--check",
            "--root",
            root,
            dir.join(target).to_str().unwrap(),
        ]);
        assert_eq!(code, 1, "{target}: the gate: {stderr}");
        // A denied path could not be fixed — before the gate's own
        // sentence, which is for the files the fixer could open.
        assert!(
            stderr.contains("error: 1 path(s) could not be fixed"),
            "{target}: {stderr}"
        );
    }
}

/// r103-cov: the `--json` `layers` object separates the CLOSED denial
/// from the OPEN developer context, which is the ONLY thing `granted`
/// says when there is no grant to show. The human row prints
/// `Grant::DENIED` for the closed case and "composition permitted" for
/// the open one; on the wire the difference is `granted` alone, and
/// nothing pinned it — collapsing `Unbound` to `granted: true` left
/// every test green while `nml binding --json` told a consumer that a
/// file the universe DENIES may compose.
#[test]
fn the_layers_object_separates_a_closed_denial_from_the_open_context() {
    let layers = |args: &[&str]| -> serde_json::Value {
        let (_, rows) = json_rows(args);
        rows.iter()
            .find(|r| r["type"] == "binding")
            .expect("a binding row")["layers"]
            .clone()
    };
    // Closed universe, no glob claims the file: denied.
    let closed = layers(&[
        "binding",
        "--json",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/docs/unclaimed.nml",
    ]);
    assert_eq!(closed, serde_json::json!({ "granted": false }), "{closed}");
    // A binding governs it but carries no `layers:` grant: denied too.
    let no_grant = layers(&[
        "binding",
        "--json",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(
        no_grant,
        serde_json::json!({ "granted": false }),
        "{no_grant}"
    );
    // The open developer context (no manifest within the fence):
    // composition is permitted and the wire says so.
    let open = layers(&[
        "binding",
        "--json",
        "--root",
        "tests/fixtures/workspace-open",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert_eq!(open, serde_json::json!({ "granted": true }), "{open}");
    // And the human rows are the two sentences the wire abbreviates.
    let (_, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/docs/unclaimed.nml",
    ]);
    assert!(
        stdout.contains("layers    none — composition denied (NML2064)\n"),
        "{stdout}"
    );
    let (_, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace-open",
        "tests/fixtures/workspace-open/x.nml",
    ]);
    assert!(stdout.contains("composition permitted"), "{stdout}");
}

/// r103-cov: the walking GATE says the budget units the walk denied.
/// A denied unit's files are purged from `Discovery::files`, so no
/// target ever carries its NML2089 row: `nml check <dir>` judged every
/// sibling, printed NOTHING about the denied subtree and exited 0 — the
/// closing `--json` row's `truncatedUnits` was the only trace, and a
/// human never sees it. The gate now reports one row per denied unit at
/// or under the named directories, in the named-file path's own
/// sentence, and the run fails like any other unjudged content.
#[cfg(unix)]
#[test]
fn the_gate_over_a_directory_reports_the_units_the_walk_denied() {
    use std::os::unix::fs::PermissionsExt;
    let dir = unit_bytes_workspace("r103-unit-gate", &["cu", "du"], 0);
    std::fs::create_dir_all(dir.join("tenants/du/locked/sub")).unwrap();
    std::fs::write(
        dir.join("tenants/du/locked/sub/hidden.flow.nml"),
        "thing h:\n    v = \"h\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.join("tenants/du/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(dir.join("tenants/du/locked"));
    if std::fs::read_dir(dir.join("tenants/du/locked")).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.to_str().unwrap();
    let denial = "tenants/du: error[NML2089]: discovery under `tenants/du` was cut short: the \
                  walk stopped at `tenants/du/locked` (unreadable: permission denied on a path \
                  component)";
    // The operator's CI invocation: the whole root, as a directory.
    let (code, stdout, stderr) = run(&["check", "--root", root, root]);
    assert_eq!(code, 1, "a denied unit fails the run: {stdout}{stderr}");
    assert!(stderr.contains(denial), "{stderr}");
    assert!(
        stderr.contains("1 skipped path(s) hold content no verb judged"),
        "the gate's tally counts it: {stderr}"
    );
    assert!(
        !stdout.contains("file(s): "),
        "no closing `N ok` line on a failed run: {stdout}"
    );
    // `validate` gates on it, and `fix --check` (the fixer's gate) too;
    // one row per denied unit, never one per file beneath it.
    for verb in [
        vec!["validate", "--root", root, root],
        vec!["fix", "--check", "--root", root, root],
    ] {
        let (code, stdout, stderr) = run(&verb);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        assert!(stderr.contains(denial), "{verb:?}: {stderr}");
        assert_eq!(
            stderr.matches("error[NML2089]").count(),
            1,
            "{verb:?}: one row per denied unit: {stderr}"
        );
    }
    // Scoped to the named directories: a run over the SIBLING unit says
    // nothing about the denied one and passes.
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        root,
        dir.join("tenants/cu").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stderr.contains("NML2089"), "{stderr}");
    // And the unit's OWN directory named as the target: its row, once,
    // and the run fails — containment is reflexive (a strict-ancestry
    // reading silenced exactly this invocation and survived the runs
    // above).
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        root,
        dir.join("tenants/du").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains(denial), "{stderr}");
    assert_eq!(stderr.matches("error[NML2089]").count(), 1, "{stderr}");
    // The row rides the wire as a diagnostic, not only as a summary fact.
    let (code, rows) = json_rows(&["check", "--json", "--root", root, root]);
    assert_eq!(code, 1);
    assert!(
        rows.iter().any(|r| r["type"] == "diagnostic"
            && r["code"] == "NML2089"
            && r["source"] == "tenants/du"),
        "{rows:?}"
    );
    assert_eq!(
        rows.last().expect("summary")["truncatedUnits"],
        serde_json::json!([
            {"unit": "tenants/du", "stop": "tenants/du/locked", "why": "unreadable"}
        ]),
        "{rows:?}"
    );
}

/// E35 (r62-sec 1c; the r60 "argv FIFO hangs" row flips): a non-regular
/// file named as the target — a FIFO here — is refused from the kernel's
/// own verdict (`Verified.kind`) BEFORE any open, by every verb, typed
/// and prompt; pre-fix the read blocked forever.
#[cfg(unix)]
#[test]
fn fifo_target_is_refused_before_open_by_every_verb() {
    let dir = workspace_copy("fifo-target");
    let pipe = dir.join("tenants/cu/fifo.flow.nml");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .expect("mkfifo runs")
            .success()
    );
    let root = dir.to_str().unwrap();
    let target = pipe.to_str().unwrap();
    let refusal = "`fifo.flow.nml` is not a regular file — a FIFO, socket or device is never \
                   opened; replace it with a regular file";
    for args in [
        vec!["check", "--root", root, target],
        vec!["validate", "--root", root, target],
        vec!["fix", "--dry-run", "--root", root, target],
    ] {
        let started = std::time::Instant::now();
        let (code, stdout, stderr) = run_bounded(&args, 20);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{args:?}"
        );
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains(refusal), "{args:?}: {stderr}");
        assert!(
            stderr.contains(&format!("failed to read {target}: ")),
            "{args:?}: {stderr}"
        );
    }
}

/// An unreadable `--schema` directory is refused in the tool's own words —
/// the reason, never `io::Error`'s `(os error N)` tail (the one refusal
/// that still carried it).
#[test]
fn an_unreadable_schema_directory_is_refused_without_an_os_error() {
    let (code, stdout, stderr) = run_in(
        &fixture("workspace-open"),
        &["check", "--schema", "nope", "x.nml"],
    );
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: --schema nope: no such directory"),
        "{stderr}"
    );
    assert!(!stderr.contains("os error"), "{stderr}");
}

/// E35 (r62-sec finding 1, the accepted delta): in a closed universe the
/// bytes read are the bytes at the KEY the kernel minted — `tenants/nope/
/// ../cu/plain.flow.nml` keys `tenants/cu/plain.flow.nml` (E28's lexical
/// pop) and reads it, `ok`. Pre-fix the path read resolved `nope/..`
/// physically and failed ENOENT while the verdict was fine; unix now
/// agrees with Win32.
#[test]
fn closed_universe_reads_the_bytes_at_the_minted_key() {
    let (code, stdout, stderr) = check_in_workspace("tenants/nope/../cu/plain.flow.nml");
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    assert!(!stderr.contains("No such file"), "{stderr}");
    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/nope/../cu/plain.flow.nml",
    ]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("file      tenants/cu/plain.flow.nml"),
        "{stdout}"
    );
}

/// E35 (RFC 0030's file-name rule, arch finding 6): a workspace manifest
/// whose file stem is not its declared `name` is a load error naming
/// both — closed-denied, every verb exits 1 — and never a second claim
/// that ambiguates the operator's manifest.
#[test]
fn manifest_with_a_foreign_stem_is_a_load_error_not_an_ambiguity() {
    let dir = workspace_copy("stem-rule");
    std::fs::copy(dir.join("demo.package.nml"), dir.join("evil.package.nml")).unwrap();
    let (code, _, stderr) = check_under(&dir, "tenants/cu/plain.flow.nml");
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "evil.package.nml: error[NML2088]: manifest failed to load: manifest file `evil.package.nml` \
             declares name `demo` — a workspace manifest is `<name>.package.nml` (expected \
             `demo.package.nml`)"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("manifests claim this file"), "{stderr}");
    let (code, stdout, stderr) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
    ]);
    // r80-cov F12: the binding stands, the run reported NML2088 — exit 1.
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("binding   tenantFlows"), "{stdout}");
    assert!(
        stderr.contains("evil.package.nml"),
        "the universe's word, once, on stderr: {stderr}"
    );
    assert!(!stdout.contains("AMBIGUOUS"), "{stdout}");
}

/// r80-cov F12: `binding`'s exit follows its closing row like every
/// verb's — an error-severity finding the run reported (a universe
/// error, NML2088 here) exits 1 even where a binding still stands. Both
/// shapes: a foreign-stem manifest (the file stays bound to the
/// operator's manifest; the run reported NML2088 for the impostor) and
/// the governing manifest past the byte cap (the file is unbound).
/// Pre-fold the bound shape exited 0 under a closing row saying
/// `errors: 1`.
#[test]
fn binding_exits_one_on_a_reported_error_even_where_a_binding_stands() {
    let dir = workspace_copy("binding-exit-stem");
    std::fs::copy(dir.join("demo.package.nml"), dir.join("evil.package.nml")).unwrap();
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["binding", "--root", &root, &plain]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("binding   tenantFlows"),
        "the binding still stands: {stdout}"
    );
    assert!(
        stderr.contains("evil.package.nml: error[NML2088]: manifest failed to load"),
        "the universe's word, once, on stderr: {stderr}"
    );
    assert!(
        !stdout.contains("NML2088"),
        "never inside a block: {stdout}"
    );
    let (code, rows) = json_rows(&["binding", "--json", "--root", &root, &plain]);
    assert_eq!(code, 1);
    assert!(rows.iter().any(|r| r["type"] == "binding"), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.to_string().contains("NML2088")),
        "{rows:?}"
    );
    let last = rows.last().unwrap();
    assert_eq!(last["exit"], 1, "{last}");
    assert!(last["errors"].as_u64().unwrap() >= 1, "{last}");

    let dir = workspace_copy("binding-exit-cap");
    let mut big = std::fs::read_to_string(dir.join("demo.package.nml")).unwrap();
    big.push_str(&comment_padding(256 * 1024));
    std::fs::write(dir.join("demo.package.nml"), big).unwrap();
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["binding", "--root", &root, &plain]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        !stdout.contains("binding   tenantFlows"),
        "the governing manifest did not load: {stdout}"
    );
    assert!(
        stderr.contains(
            "demo.package.nml: error[NML2088]: manifest failed to load: too large: over 256 KiB"
        ),
        "{stdout}{stderr}"
    );
    let (code, rows) = json_rows(&["binding", "--json", "--root", &root, &plain]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["exit"], 1, "{last}");
    assert!(last["errors"].as_u64().unwrap() >= 1, "{last}");
}

/// r80-cov F10: a non-UTF-8 argument is a usage error — exit 2, under
/// `--json` an `error` row of kind `usage` then the closing row — never
/// a panic (`std::env::args` panicked on it: exit 101, a backtrace).
#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_a_usage_error_not_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let bad = std::ffi::OsStr::from_bytes(b"\xff.nml");
    let out = nml_bin().arg("check").arg(bad).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("argument 2 is not valid UTF-8"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    let out = nml_bin()
        .arg("check")
        .arg("--json")
        .arg(bad)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
    let err = rows
        .iter()
        .find(|r| r["type"] == "error")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(err["kind"], "usage", "{err}");
    assert_eq!(rows.last().unwrap()["exit"], 2, "{rows:?}");
}

/// E35 (arch finding 2): `fix` continues past a bad argument — the
/// failure is reported through the shared reporter, the other files are
/// fixed, the summary prints, and the run exits 1 for the failure.
/// Pre-fix `fix a <absent> b` fixed nothing and printed no summary. A
/// target OUTSIDE the root is not a bad argument the run continues past
/// but the invocation's own mistake (r85 D3): exit 2, nothing runs.
#[cfg(unix)]
#[test]
fn fix_continues_past_a_bad_argument() {
    let dir = workspace_copy("fix-continue");
    let a = dir.join("tenants/cu/plain.flow.nml");
    let b = dir.join("tenants/cu/bad.flow.nml");
    let absent = dir.join("tenants/cu/nope.flow.nml");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        a.to_str().unwrap(),
        absent.to_str().unwrap(),
        b.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "error: {}: no such file or directory",
            absent.display()
        )),
        "{stderr}"
    );
    assert!(
        stdout.contains("of 3 file(s)"),
        "the run completed: {stdout}"
    );
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        a.to_str().unwrap(),
        "/etc/hosts",
        b.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: /etc/hosts is outside the workspace root"),
        "{stderr}"
    );
    assert!(!stdout.contains("file(s)"), "nothing ran: {stdout}");
    assert!(!stdout.contains("would fix"), "nothing ran: {stdout}");
}

/// A directory argument is expanded AT THE KERNEL'S KEY (r64 finding 4,
/// option (a)): `proj/nope/../vendor` is `Dir(vendor)` by E28's lexical
/// pop and enumerates `vendor` — there is no OS walk of the typed
/// spelling to fail (`read_dir` would have resolved `nope/..`
/// physically and failed), and the same file reached through two
/// spellings (`nope/../vendor`, `vendor`) is fixed ONCE.
#[cfg(unix)]
#[test]
fn fix_expands_a_dotdot_directory_argument_at_the_kernels_key() {
    let dir = workspace_copy("fix-walk-arg");
    std::fs::create_dir_all(dir.join("tenants/cu/real")).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        dir.join("tenants/cu/real/r.flow.nml"),
    )
    .unwrap();
    let parent = dir.as_ref().parent().unwrap().to_path_buf();
    let proj = dir
        .as_ref()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let (code, stdout, stderr) = run_in(
        &parent,
        &[
            "fix",
            "--dry-run",
            "--root",
            &proj,
            &format!("{proj}/tenants/cu/real"),
            &format!("{proj}/nope/../vendor"),
            &format!("{proj}/vendor"),
        ],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        !stderr.contains("failed to read") && !stderr.contains("could not be fixed"),
        "no walk ran, so no walk failed: {stderr}"
    );
    assert!(
        stdout.contains("of 2 file(s)"),
        "`tenants/cu/real/r.flow.nml` and `vendor/base.flow.nml` once each — the `..` \
         spelling and the plain one reach the same key: {stdout}"
    );
}

/// A dot-file is never among the files a directory argument expands to
/// (the kernel's enumeration hides what the filesystem hides):
/// `.hidden.flow.nml` beside `plain.flow.nml` is neither rewritten nor
/// listed by `fix tenants/cu`, while naming it on the command line fixes
/// it — the operator asked.
#[test]
fn fix_never_expands_a_directory_to_a_dot_file() {
    let dir = workspace_copy("fix-dot-file");
    let hidden = dir.join("tenants/cu/.hidden.flow.nml");
    // A fixable typo (`vv` → `v`); `bad.flow.nml`'s own did-you-mean
    // (`w` → `v`) would repeat `v` and the parse gate refuses it.
    std::fs::write(&hidden, "thing a:\n    vv = \"x\"\n").unwrap();
    let root = dir.to_str().unwrap();
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        root,
        dir.join("tenants/cu").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("of 4 file(s)"),
        "bad, member-lookup, plain and the project config — not the dot-file: {stdout}"
    );
    assert!(
        !stdout.contains(".hidden") && !stderr.contains(".hidden"),
        "{stdout}{stderr}"
    );
    let (code, stdout, stderr) =
        run(&["fix", "--dry-run", "--root", root, hidden.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("of 1 file(s)"),
        "named, it is the operator's: {stdout}"
    );
    assert!(
        stdout.contains("would fix ") && stdout.contains(".hidden.flow.nml (1 edit(s))"),
        "{stdout}"
    );
}

/// r67 (r66 finding F1): the walk-error line of a failed directory
/// argument prints through `sanitized` — an operator-typed argument
/// carrying an ESC byte (`proj/<ESC>[31mX/../vendor`, `Dir(vendor)` by
/// E28's pop, `read_dir` fails on `<ESC>[31mX/..`) is reported in the
/// escaped spelling and never as a raw terminal escape. The
/// `sanitized(&e)` → `{e}` mutant survived every pre-r67 pin (r66
/// mutation (c)); it is RED on this one.
#[cfg(unix)]
#[test]
fn fix_reports_a_hostile_argument_through_the_sanitizer() {
    let dir = workspace_copy("fix-walk-arg-hostile");
    let parent = dir.as_ref().parent().unwrap().to_path_buf();
    let proj = dir
        .as_ref()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // A hostile DIRECTORY spelling is no longer an error at all: the
    // kernel classifies it at its key (`vendor`) and it expands there,
    // deduplicated against the plain spelling of the same directory.
    let hostile_dir = format!("{proj}/\u{1b}[31mX/../vendor");
    let (code, stdout, stderr) = run_in(
        &parent,
        &[
            "fix",
            "--dry-run",
            "--root",
            &proj,
            &hostile_dir,
            &format!("{proj}/vendor"),
        ],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        !stdout.contains('\u{1b}') && !stderr.contains('\u{1b}'),
        "a raw ESC reached the terminal: {stdout:?} {stderr:?}"
    );
    assert!(stdout.contains("of 1 file(s)"), "{stdout}");
    // A hostile FILE spelling that fails is reported through the
    // sanitizer, per argument, the run continuing.
    let hostile_file = format!("{proj}/\u{1b}[31mX/../nope.flow.nml");
    let (code, stdout, stderr) = run_in(
        &parent,
        &[
            "fix",
            "--dry-run",
            "--root",
            &proj,
            &hostile_file,
            &format!("{proj}/vendor"),
        ],
    );
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        !stderr.contains('\u{1b}'),
        "a raw ESC reached stderr: {stderr:?}"
    );
    assert!(
        stderr.contains(&format!(
            "error: {proj}/\\u{{1b}}[31mX/../nope.flow.nml: no such file or directory"
        )),
        "the escaped spelling, as `sanitized` renders ESC: {stderr}"
    );
    assert!(
        stdout.contains("of 2 file(s)"),
        "the failed candidate and the other argument are both counted, the summary printed: {stdout}"
    );
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
}

/// E35 (arch finding 2): `fix` prints the universe's word on a file —
/// the inert-input note (NML2080) on its chain — like every other verb;
/// pre-fix it printed the errors and swallowed the notes.
#[test]
fn fix_prints_inert_input_notes_like_check() {
    let (code, _, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("tenants/cu/nml-project.nml: warning[NML2080]: project config"),
        "{stderr}"
    );
}

/// E35 (r62-sec 1b): a closed-universe `fix` writes THROUGH the chain's
/// parent descriptor (`write_beneath`), never by path — the file is
/// rewritten in place with its permission bits preserved and no temp
/// file left behind.
#[cfg(unix)]
#[test]
fn closed_universe_fix_writes_through_the_parent_handle() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("fix-write-beneath");
    let file = dir.join("tenants/cu/legacy.flow.nml");
    std::fs::write(
        &file,
        "oneof email by kind:\n    \"log\" => emailLog\n\nmodel emailLog:\n    path string?\n",
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (code, stdout, stderr) = run(&[
        "fix",
        "--root",
        dir.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("(1 edit(s))"), "{stdout}");
    let fixed = std::fs::read_to_string(&file).unwrap();
    assert!(fixed.contains("\"log\" -> emailLog"), "{fixed}");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let leftovers: Vec<String> = std::fs::read_dir(dir.join("tenants/cu"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// r105-cov (closes r103-cov M65): a `fix` whose WRITE fails part-way
/// leaves the original byte-for-byte and no temp file behind — the temp
/// is unlinked on the failed write, and the original was never opened
/// for writing. The failure is `RLIMIT_FSIZE`: a file-size limit of one
/// block makes the write of a fixed text larger than that fail with
/// `EFBIG` — portably on every unix, once `SIGXFSZ` (25 on Linux and the
/// BSDs) is ignored so the process sees the error instead of dying — the
/// same `write(2)` failure a full disk (`ENOSPC`) hands the fixer.
#[cfg(unix)]
#[test]
fn a_fix_whose_write_fails_leaves_the_original_and_no_temp_behind() {
    let dir = workspace_copy("fix-write-fails");
    let file = dir.join("tenants/cu/legacy.flow.nml");
    // Past one block whatever the shell's unit (1024 bytes on Linux's
    // `ulimit -f`, 512 elsewhere): 8 KiB of comment beside the fixable
    // arm, which the fixer copies through.
    let padding: String = (0..128)
        .map(|i| format!("// padding line {i:03} .......................................\n"))
        .collect();
    let source = format!(
        "{padding}oneof email by kind:\n    \"log\" => emailLog\n\nmodel emailLog:\n    path string?\n"
    );
    std::fs::write(&file, &source).unwrap();
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("trap '' 25; ulimit -f 1 || exit 99; exec \"$0\" fix --root \"$1\" \"$2\"")
        .arg(env!("CARGO_BIN_EXE_nml"))
        .arg(dir.to_str().unwrap())
        .arg(file.to_str().unwrap())
        .env("NML_UNICODE", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(99),
        "the shell could not lower the file-size limit"
    );
    assert!(
        out.status.code().is_some(),
        "killed by a signal instead of refusing: {:?}",
        out.status
    );
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains("1 path(s) could not be fixed") && stderr.contains("File too large"),
        "the failed WRITE is the run's verdict, by name: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        source,
        "the original must be untouched"
    );
    let leftovers: Vec<String> = std::fs::read_dir(dir.join("tenants/cu"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ── RFC 0019 item 0, round 69a: the r68 fold (security A1/A4, UX B1–B6) ──

/// A1 (r68 security F1): the SUCCESS line prints through the sanitizer.
/// A tenant-named file carrying a LF (`nl<LF> admin-secret.flow.nml`)
/// forged a second `: ok` line in the operator's CI log, and one
/// carrying ESC/BEL smuggled a terminal title change — on stdout, where
/// no other surface was unsanitized. Now `check` and `validate` print
/// exactly ONE `: ok` line with no raw control byte; the `sanitized` →
/// identity mutant at the success line is RED here.
#[test]
fn success_line_never_carries_raw_control_bytes() {
    let dir = workspace_copy("ok-line-hostile");
    for name in [
        "nl\n admin-secret.flow.nml",
        "ev\u{1b}]0;pwned\u{7}il.flow.nml",
    ] {
        let path = dir.join("tenants/cu").join(name);
        std::fs::write(&path, "thing a:\n    v = \"x\"\n").unwrap();
        for verb in ["check", "validate"] {
            let (code, stdout, stderr) = run(&[
                verb,
                "--root",
                dir.to_str().unwrap(),
                path.to_str().unwrap(),
            ]);
            assert_eq!(code, 0, "{verb} {name:?}: {stdout}{stderr}");
            assert!(
                !stdout.bytes().any(|b| b < 0x20 && b != b'\n'),
                "{verb} {name:?}: raw control byte on stdout: {stdout:?}"
            );
            assert_eq!(
                stdout.matches(": ok").count(),
                1,
                "{verb} {name:?}: exactly one ok line: {stdout:?}"
            );
            assert_eq!(stdout.lines().count(), 1, "{verb} {name:?}: {stdout:?}");
            assert!(
                stdout.contains("\\n admin-secret") || stdout.contains("\\u{1b}]0;pwned\\u{7}il"),
                "{verb} {name:?}: the escaped spelling: {stdout:?}"
            );
        }
    }
}

/// A4: the check TARGET is capped at 16 MiB (a decision-with-default;
/// the discovery caps never covered it, and a tenant-committed 100 MB
/// `.flow.nml` cost the operator's CI ~200× its size in memory). A
/// 17 MiB sparse target is refused fast, naming the cap, by every
/// reading verb; a 1 MiB target is fine.
#[test]
fn check_target_over_sixteen_mib_is_refused_naming_the_cap() {
    let dir = workspace_copy("target-cap");
    let big = dir.join("tenants/cu/big.flow.nml");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(17 * 1024 * 1024).unwrap();
    drop(file);
    for verb in ["check", "validate", "fix"] {
        let mut args = vec![verb];
        if verb == "fix" {
            args.push("--dry-run");
        }
        args.extend(["--root", dir.to_str().unwrap(), big.to_str().unwrap()]);
        let started = std::time::Instant::now();
        let (code, stdout, stderr) = run(&args);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{verb}"
        );
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.contains("a check target is read only up to 16 MiB (16777216 bytes)")
                && stderr.contains("17 MiB (17825792 bytes)"),
            "{verb}: {stderr}"
        );
        assert!(!stdout.contains(": ok"), "{verb}: {stdout}");
    }
    let fine = dir.join("tenants/cu/fine.flow.nml");
    let mut text = String::from("thing a:\n    v = \"x\"\n");
    text.push_str(&comment_padding(1024 * 1024));
    std::fs::write(&fine, text).unwrap();
    let (code, stdout, stderr) = check_under(&dir, "tenants/cu/fine.flow.nml");
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
}

/// B1 (r68 UX F2/F13): `--help`/`-h` is help — stdout, exit 0, stderr
/// silent — for every verb, BEFORE any filesystem access: a bad `--root`
/// and an absent target beside it change nothing (pre-fold `nml check
/// --help` walked the working directory's universe as a file named
/// `--help`). The hidden oracle flag is never listed. `nml --help` is
/// stdout too, and a near-miss verb gets the crate's did-you-mean.
#[test]
fn per_verb_help_is_stdout_exit_zero_before_any_filesystem_access() {
    for (args, usage) in [
        (
            vec![
                "check",
                "--root",
                "no/such/dir",
                "--help",
                "no/such/file.nml",
            ],
            "usage: nml check [--root <dir>] [--schema <dir>] [--strict] \
             [--max-findings <n>] [--json] [--quiet] <path>...",
        ),
        (
            vec!["validate", "-h"],
            "usage: nml validate [--root <dir>] [--max-findings <n>] [--json] [--quiet] \
             <path>...",
        ),
        (
            vec!["fix", "--bogus", "--help"],
            "usage: nml fix [--root <dir>] [--schema <dir>] [--dry-run] [--check] \
             [--max-findings <n>] [--json] [--quiet] <path>...",
        ),
        (
            vec!["binding", "-h"],
            "usage: nml binding [--root <dir>] [--json] [--quiet] <file>...",
        ),
        (
            vec!["parse", "--help"],
            "usage: nml parse [--json] [--quiet] <file>",
        ),
        (
            vec!["fmt", "-h"],
            "usage: nml fmt [--root <dir>] [--dry-run] [--check] [--json] [--quiet] <path>...",
        ),
        (
            vec!["explain", "--help"],
            "usage: nml explain [--json] [--quiet] <code>... | --list",
        ),
        (
            vec!["limits", "--help"],
            "usage: nml limits [--json] [--quiet]",
        ),
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stdout}{stderr}");
        assert!(stdout.starts_with(usage), "{args:?}: {stdout}");
        assert!(stdout.contains("-h, --help"), "{args:?}: {stdout}");
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
    }
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("USAGE:") && stdout.contains("    fix <path>..."),
        "{stdout}"
    );
    let (commands, options) = stdout.split_once("OPTIONS").expect("an OPTIONS section");
    assert!(
        !commands.contains("--root") && options.contains("--root <dir>"),
        "--root is an option, not a command: {stdout}"
    );
    assert!(stderr.is_empty(), "{stderr}");
    // An unknown command, like every usage error, exits 2.
    let (code, stdout, stderr) = run(&["chekc"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("error: unknown command: chekc (did you mean `check`?)"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&["bogus"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: unknown command: bogus\n") && stderr.contains("COMMANDS:"),
        "{stderr}"
    );
    // A flag no verb accepts is rejected by every verb (the one
    // behavioural delta of the shared parser; pre-fold `check --bogus x`
    // read a file called `--bogus`).
    let (code, _, stderr) = run(&["check", "--bogus", "x.nml"]);
    assert_eq!(code, 2, "a usage error: {stderr}");
    assert!(
        stderr.contains("error: unknown flag --bogus; usage: nml check"),
        "{stderr}"
    );
}

/// B2 (r68 UX F6): a universe note prints ONCE per run. A directory
/// `fix` walk resolves every file under the inert `tenants/cu/nml-
/// project.nml` (pre-fold: one NML2080 line per file), and a multi-
/// target `check` did the same per target. The dedup is on `(source,
/// code, message)`; removing it is RED here.
#[test]
fn universe_notes_print_once_per_run() {
    let dir = workspace_copy("notes-once");
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("of 4 file(s)"), "{stdout}");
    assert_eq!(
        stderr.matches("warning[NML2080]").count(),
        1,
        "one note for the whole walk: {stderr}"
    );
    let (code, stdout, stderr) = run(&[
        "check",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
        dir.join("tenants/cu/member-lookup.flow.nml")
            .to_str()
            .unwrap(),
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(stderr.matches("warning[NML2080]").count(), 1, "{stderr}");
    assert_eq!(stdout.matches(": ok").count(), 1, "{stdout}");
    assert!(stderr.contains("error[NML2064]"), "{stderr}");
    assert!(
        stderr.trim_end().ends_with("error: 1 of 2 file(s) failed"),
        "{stderr}"
    );
}

/// B3 (r68 UX F7): ONE explain-hint rule — once per run, the first
/// error's code, else the first warning's — for every verb. `validate`
/// on a warning-only run printed no hint (its universe pass dropped the
/// accumulator); a multi-file `fix` printed one per rejected file.
#[test]
fn explain_hint_prints_once_per_run_for_every_verb() {
    let dir = workspace_copy("hint-once");
    let (code, stdout, stderr) = run(&[
        "validate",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.contains("warning[NML2080]"), "{stderr}");
    assert_eq!(
        stderr
            .matches("for more information, run: nml explain NML2080")
            .count(),
        1,
        "{stderr}"
    );
    assert!(
        stdout.contains(": ok (symbols only — run nml check for schema validation)"),
        "{stdout}"
    );
    // The first ERROR wins over the earlier warning, once, across
    // targets.
    let (code, _, stderr) = run(&[
        "check",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
        dir.join("tenants/cu/member-lookup.flow.nml")
            .to_str()
            .unwrap(),
        dir.join("tenants/cu/bad.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.matches("for more information").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML2064"),
        "{stderr}"
    );
}

/// B3 for a multi-file `fix`: two arguments a closed binding rejects
/// (NML2083 through the same link) print the hint ONCE, after both.
#[cfg(unix)]
#[test]
fn fix_prints_the_explain_hint_once_across_rejected_files() {
    let dir = scratch_dir("fix-hint-once");
    copy_tree(&fixture("workspace-link-a"), &dir);
    let (code, stdout, stderr) = run(&[
        "fix",
        "--dry-run",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/lib/base.flow.nml").to_str().unwrap(),
        dir.join("tenants/cu/lib/x.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(
        code, 1,
        "two refused paths could not be fixed: {stdout}{stderr}"
    );
    assert!(
        stderr.contains("error: 2 path(s) could not be fixed"),
        "{stderr}"
    );
    assert_eq!(stderr.matches("error[NML2083]").count(), 2, "{stderr}");
    assert_eq!(
        stderr
            .matches("for more information, run: nml explain NML2083")
            .count(),
        1,
        "{stderr}"
    );
    assert!(
        stdout.contains("2 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
}

/// B4 (r68 UX F5): an absent target says so — by the KERNEL's verdict
/// (no leaf at the minted key), never an OS error, never "not a
/// directory". `validate` and `check` fail; `binding` shows the binding
/// that WOULD govern the path and says the file is absent.
#[test]
fn absent_targets_are_described_not_leaked() {
    let dir = workspace_copy("absent");
    let nope = dir.join("tenants/cu/nope.flow.nml");
    for verb in ["check", "validate"] {
        let (code, stdout, stderr) = run(&[
            verb,
            "--root",
            dir.to_str().unwrap(),
            nope.to_str().unwrap(),
        ]);
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.contains(&format!(
                "error: {}: no such file or directory",
                nope.display()
            )),
            "{verb}: {stderr}"
        );
        assert!(!stderr.contains("os error"), "{verb}: {stderr}");
    }
    let (code, stdout, stderr) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        nope.to_str().unwrap(),
    ]);
    assert_eq!(
        code, 0,
        "bound (the binding that would govern it): {stdout}{stderr}"
    );
    assert!(stdout.contains("binding   tenantFlows"), "{stdout}");
    assert!(
        stdout.contains(
            "tenants/cu/nope.flow.nml: warning: no such file — the binding shown is what WOULD \
             govern this path"
        ),
        "{stdout}"
    );
    // No `--root`, absent directory: derivation cannot find a directory
    // to fence — the file cannot exist; say so, and where to name the
    // universe.
    let (code, _, stderr) = run(&["check", "nope/x.nml"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains(
            "error: nope/x.nml: no such file or directory (its directory does not exist either — check \
             the path; a workspace root is derived from the target's directory, so pass --root \
             <dir> to name one instead)"
        ),
        "{stderr}"
    );
}

/// A directory target is WALKED by the checking verbs — the kernel's one
/// enumeration, so `check tenants/` and `fix tenants/` expand one
/// spelling to one file list — with every file reported on its own; `.`
/// included. (A verb that refused a directory and sent the operator to
/// `nml fix --dry-run` — a different tool: diffs, exit 0 on errors — or
/// to a shell `**` glob, which checks one level deep in a bash without
/// `globstar`, was the wrong first run.)
#[test]
fn directory_targets_are_walked_by_the_checking_verbs() {
    let dir = workspace_copy("dir-target");
    let tenants = dir.join("tenants");
    let root = dir.to_str().unwrap();
    // `check`: every `.flow.nml` under tenants/ — the clean one reports
    // ok, the broken one its findings, and the run exits 1 for it.
    let (code, stdout, stderr) = run(&["check", "--root", root, tenants.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("tenants/cu/plain.flow.nml: ok (1 declaration(s))"),
        "{stdout}"
    );
    assert!(
        stderr.contains("tenants/cu/bad.flow.nml:2:9: error[NML2008]"),
        "{stderr}"
    );
    assert!(!stderr.contains("is a directory"), "{stderr}");
    // `validate` (symbols only) over the same tree is clean.
    let (code, stdout, stderr) = run(&["validate", "--root", root, tenants.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("tenants/cu/bad.flow.nml: ok (symbols only")
            && stdout.contains("tenants/cu/plain.flow.nml: ok (symbols only"),
        "{stdout}"
    );
    // `.` from the root walks the whole workspace: the ambiguously
    // claimed shared file fails it, the clean tenant file passes.
    let (code, stdout, stderr) = run_in(&dir, &["check", "."]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("plain.flow.nml: ok"), "{stdout}");
    assert!(stderr.contains("error[NML2087]"), "{stderr}");
    assert!(!stderr.contains("is a directory"), "{stderr}");
    assert!(!stderr.contains("not a relative path"), "{stderr}");
    // A directory holding no `.nml` file is named in the refusal.
    std::fs::create_dir_all(dir.join("nonml")).unwrap();
    std::fs::write(dir.join("nonml/README.md"), "no nml here\n").unwrap();
    let (code, _, stderr) = run(&["check", "--root", root, dir.join("nonml").to_str().unwrap()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error: no .nml files found under `") && stderr.contains("nonml`"),
        "{stderr}"
    );
    // The `--json` stream keeps its shape: one result row per walked
    // file, then the summary.
    let (code, rows) = json_rows(&["check", "--json", "--root", root, tenants.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(
        rows.iter().filter(|r| r["type"] == "result").count() >= 2,
        "{rows:#?}"
    );
    assert_eq!(rows.last().unwrap()["type"], "summary");
}

/// B4 (r68 UX F8): a rejection of a path spelled through `..` names the
/// spelling AS TYPED beside the key, so the component it names (`lib`)
/// is visible in the sentence; a plain spelling keeps the certified
/// key-only text (the `check-symlink` golden).
#[cfg(unix)]
#[test]
fn symlink_rejection_through_dotdot_names_the_typed_spelling_and_the_key() {
    let dir = scratch_dir("dotdot-spelling");
    copy_tree(&fixture("workspace-link-a"), &dir);
    let typed = dir.join("tenants/cu/lib/../plain.flow.nml");
    let (code, _, stderr) = run(&[
        "check",
        "--root",
        dir.to_str().unwrap(),
        typed.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "tenants/cu/plain.flow.nml: error[NML2083]: closed binding rejects `{}` (key \
             `tenants/cu/plain.flow.nml`): path component `lib` is a symlink",
            typed.display()
        )),
        "{stderr}"
    );
    // A rejected path has no verified leaf, but `binding` never calls it
    // absent: the rejection is the whole story (and the link's target is
    // never observed).
    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/lib/base.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(
        code, 1,
        "rejected = unbound, its finding under notes: {stdout}"
    );
    assert!(stdout.contains("error[NML2083]"), "{stdout}");
    assert!(!stdout.contains("no such file"), "{stdout}");
}

/// B6: `--json` is line-delimited JSON on stdout — one object per line,
/// `type`-discriminated, stderr silent, exit codes unchanged — for
/// every workspace verb, usage errors included; a `result`/`summary`
/// row makes the closing `error` row redundant, so it is dropped.
#[test]
fn json_output_is_ndjson_on_stdout_with_a_silent_stderr() {
    let dir = workspace_copy("json");
    let rows = |args: &[&str]| -> (i32, Vec<serde_json::Value>) {
        let (code, stdout, stderr) = run(args);
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
        (code, rows_of(&stdout))
    };
    let root = dir.to_str().unwrap().to_string();
    let member = dir
        .join("tenants/cu/member-lookup.flow.nml")
        .display()
        .to_string();
    let (code, r) = rows(&["check", "--json", "--root", &root, &member]);
    assert_eq!(code, 1);
    assert_eq!(r[0]["type"], "diagnostic");
    assert_eq!(r[0]["code"], "NML2080");
    assert_eq!(r[0]["severity"], "warning");
    assert_eq!(r[1]["code"], "NML2064");
    assert_eq!(r[1]["line"], 4);
    assert_eq!(r[1]["col"], 7);
    // r73-cli claim 1: the run's closing `summary` row is the LAST row
    // of every verb on every path; the per-target `result` row is the
    // last row FOR ITS TARGET.
    let last = r.last().unwrap();
    assert_eq!(last["type"], "summary");
    assert_eq!(last["verb"], "check");
    assert_eq!(last["exit"], 1);
    let result = r[r.len() - 2].clone();
    assert_eq!(result["type"], "result");
    assert_eq!(result["ok"], false);
    assert_eq!(result["errors"], 1);
    assert_eq!(result["key"], "tenants/cu/member-lookup.flow.nml");
    assert!(
        r.iter().all(|row| row["type"] != "error"),
        "no closing error row: {r:?}"
    );

    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, r) = rows(&["validate", "--json", "--root", &root, &plain]);
    assert_eq!(code, 0);
    let last = r.last().unwrap();
    assert_eq!(last["type"], "summary");
    assert_eq!(last["verb"], "validate");
    assert_eq!(last["exit"], 0);
    let result = r[r.len() - 2].clone();
    assert_eq!(
        (result["type"].as_str(), result["ok"].as_bool()),
        (Some("result"), Some(true))
    );
    assert_eq!(result["verb"], "validate");
    assert!(result["declarations"].is_null());

    let (code, r) = rows(&["binding", "--json", "--root", &root, &plain]);
    assert_eq!(code, 0);
    assert_eq!(r.len(), 2);
    assert_eq!(r[1]["type"], "summary");
    assert_eq!(r[1]["exit"], 0);
    assert_eq!(r[0]["type"], "binding");
    assert_eq!(r[0]["governing"], "bound");
    assert_eq!(r[0]["binding"]["name"], "tenantFlows");
    assert_eq!(r[0]["binding"]["glob"]["index"], 0);
    assert_eq!(r[0]["layers"]["granted"], false);
    assert_eq!(r[0]["universe"], "closed");
    assert_eq!(r[0]["root"]["origin"], "explicit");
    assert_eq!(r[0]["notes"][0]["code"], "NML2080");

    let typo = dir.join("tenants/cu/typo.flow.nml");
    std::fs::write(&typo, "thing b:\n    vv = \"x\"\n").unwrap();
    let typo = typo.display().to_string();
    let (code, r) = rows(&["fix", "--json", "--dry-run", "--root", &root, &typo]);
    assert_eq!(code, 0);
    let fix = r
        .iter()
        .find(|row| row["type"] == "fix")
        .expect("a fix row");
    assert_eq!(fix["applied"], 1);
    assert!(fix["diff"].as_str().unwrap().starts_with("--- a/"), "{fix}");
    let summary = r.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["edits"], 1);

    // Usage errors are rows too — exit 2 in every verb.
    let (code, r) = rows(&["check", "--json"]);
    assert_eq!(code, 2);
    assert_eq!(r[0]["type"], "error");
    assert_eq!(r[0]["exit"], 2);
    let (code, r) = rows(&["binding", "--json"]);
    assert_eq!(code, 2);
    assert_eq!(r[0]["exit"], 2);
}

/// r70: a `--json` consumer that closes the pipe early (`| head -1`)
/// never sees a panic — stderr stays silent and the run exits 1, the
/// reporter's refusal (pre-fix `println!` panicked with a backtrace on
/// stderr, exit 101). The run owes MORE than a pipe buffer (512 files,
/// well past 64 KiB of rows) so the child is BLOCKED on a write when
/// the reader closes — r80-cov F7: 64 copies of one path deduplicated
/// to a single ~1 KiB target that fit the buffer whole, and the child
/// exited 0 whenever it outran the reader. The consumer here reads
/// exactly the contract row — the whole first line, asserted verbatim
/// — and closes: the header is what `| head -1` gets.
#[test]
fn json_rows_on_a_closed_pipe_never_panic() {
    use std::io::{BufRead as _, Read as _};
    use std::process::{Command, Stdio};
    let dir = workspace_copy("json-epipe");
    let root = dir.to_str().unwrap().to_string();
    let plain = std::fs::read(dir.join("tenants/cu/plain.flow.nml")).unwrap();
    for i in 0..512 {
        std::fs::write(dir.join(format!("tenants/cu/epipe{i}.flow.nml")), &plain).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_nml"))
        .args(["check", "--json", "--root", &root])
        .arg(dir.join("tenants/cu"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("nml runs");
    // Read the contract row — the first line — then close the read end
    // while rows are still owed.
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut header = String::new();
    let _ = stdout.read_line(&mut header);
    assert_eq!(header.trim_end(), contract_line(), "{header}");
    drop(stdout);
    let status = child.wait().expect("waits");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.is_empty(),
        "stderr must stay silent under --json: {stderr}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "EPIPE ends the run with exit 1 — the reporter's refusal, never a panic (101)"
    );
}

/// r70 pin gap (mutation M1 survived): a UNIVERSE error under `--json`
/// is rows only — the NML2089 diagnostic row then the `error` row —
/// with a silent stderr, for every workspace verb.
#[cfg(unix)]
#[test]
fn json_universe_error_is_rows_only_with_a_silent_stderr() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("json-universe");
    // r75: in the ROOT unit (`vendor/` is content no glob reaches) — a
    // locked `tenants/<x>` is a budget unit of its own now, denied alone.
    let locked = dir.join("vendor/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return; // root: the lock does not bite
    }
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    for verb in [&["check"][..], &["validate"], &["fix", "--dry-run"]] {
        let mut args: Vec<&str> = verb.to_vec();
        args.extend(["--json", "--root", &root, &plain]);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        assert!(
            stderr.is_empty(),
            "{verb:?}: stderr must be silent: {stderr}"
        );
        let rows = rows_of(&stdout);
        assert_eq!(rows[0]["type"], "diagnostic", "{verb:?}");
        assert_eq!(rows[0]["code"], "NML2089", "{verb:?}");
        assert_eq!(rows[rows.len() - 2]["type"], "error", "{verb:?}");
        // r73-cli claim 1: the closing row, on the universe-error path
        // too — the one row a consumer can always read the exit from.
        let last = rows.last().unwrap();
        assert_eq!(last["type"], "summary", "{verb:?}");
        assert_eq!(last["exit"], 1, "{verb:?}");
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// r70 pin gap (mutation M6 survived): many targets share ONE universe,
/// fixed from the FIRST target when `--root` is absent — a later target
/// outside it fails as `outside the workspace root`, never re-roots the
/// run. (A universe derived from the last target would report the first
/// target as the outsider.)
#[test]
fn many_targets_share_the_first_targets_universe() {
    let dir = workspace_copy("first-target-universe");
    let elsewhere = scratch_dir("first-target-elsewhere");
    std::fs::write(elsewhere.join("o.nml"), "thing t:\n    a = 1\n").unwrap();
    let first = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let second = elsewhere.join("o.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", &first, &second]);
    // The invocation's mistake (r85 D3): exit 2, and nothing checks —
    // not even the first target, which the universe was derived from.
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        !stdout.contains("plain.flow.nml: ok ("),
        "nothing ran: {stdout}"
    );
    assert!(
        stderr.contains(&format!("error: {second} is outside the workspace root")),
        "the SECOND target is the outsider: {stderr}"
    );
    assert!(!stderr.contains(&format!("{first} is outside")), "{stderr}");
}

/// r70 F6(b): a read failure among many targets is reported ONCE by
/// its target — `error: failed to read <target>: …`, the single-target
/// shape — never `error: <target>: failed to read <target>: …` (pre-fix
/// `run_targets` recognised only an error that STARTED with the
/// target's spelling). Human and `--json` alike.
#[cfg(unix)]
#[test]
fn many_targets_read_failure_names_its_target_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("read-failure-once");
    let plain = dir.join("tenants/cu/plain.flow.nml");
    let locked = dir.join("tenants/cu/locked.flow.nml");
    std::fs::copy(&plain, &locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.to_str().unwrap().to_string();
    let plain = plain.display().to_string();
    let locked = locked.display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &plain, &locked]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("plain.flow.nml: ok ("), "{stdout}");
    assert!(
        stderr.contains(&format!("error: failed to read {locked}: ")),
        "the single-target shape: {stderr}"
    );
    assert!(
        !stderr.contains(&format!("{locked}: failed to read")),
        "named once, never `<target>: failed to read <target>`: {stderr}"
    );
    assert_eq!(stderr.matches(&locked).count(), 1, "{stderr}");
    assert!(stderr.contains("error: 1 of 2 file(s) failed"), "{stderr}");
    let (code, stdout, stderr) = run(&["check", "--json", "--root", &root, &plain, &locked]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    let last: serde_json::Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
    assert_eq!(last["type"], "summary");
    assert_eq!(last["exit"], 1);
    let last: serde_json::Value = serde_json::from_str(lines[lines.len() - 2]).unwrap();
    assert_eq!(last["type"], "error");
    let message = last["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("failed to read {locked}: ")),
        "{message}"
    );
    assert_eq!(message.matches(&locked).count(), 1, "{message}");
}

/// B5: the three previously uncoded universe findings carry stable
/// codes and `nml explain` serves each with a why, an example and a
/// fix; NML2064's page no longer claims the ambiguous shape.
#[test]
fn universe_findings_are_coded_and_explained() {
    for (code, needle) in [
        ("NML2087", "claim this file"),
        ("NML2088", "failed to load"),
        ("NML2089", "cannot enumerate"),
    ] {
        let (exit, stdout, stderr) = run(&["explain", code]);
        assert_eq!(exit, 0, "{code}: {stderr}");
        assert!(stdout.starts_with(&format!("# {code}")), "{stdout}");
        assert!(stdout.contains(needle), "{code}: {stdout}");
        assert!(stdout.contains("**Fix:**"), "{code}: {stdout}");
        assert!(stdout.contains("```"), "{code} has an example: {stdout}");
    }
    let (_, stdout, _) = run(&["explain", "NML2064"]);
    assert!(!stdout.contains("ambiguously claimed"), "{stdout}");
    assert!(
        stdout.contains("NML2087"),
        "points at the ambiguous code: {stdout}"
    );
}

/// r69b (arch r68 finding 3): the kernel's symlink verdict SURFACED —
/// `nml binding` in an OPEN universe says a followed link is a link
/// (an `info` notes row naming the linked component, 1-based, of the
/// root-relative path), the key names the target; a plain path gets no
/// such row; a CLOSED universe never reaches it — its NML2083 is the
/// whole story.
#[cfg(unix)]
#[test]
fn binding_reports_a_followed_symlink_in_open_universes_only() {
    let dir = scratch_dir("via-symlink");
    std::fs::create_dir_all(dir.join("real/sub")).unwrap();
    std::fs::write(dir.join("real/sub/x.nml"), "thing t:\n    a = 1\n").unwrap();
    std::fs::write(dir.join("real/plain.nml"), "thing u:\n    a = 1\n").unwrap();
    std::os::unix::fs::symlink("real", dir.join("link")).unwrap();
    std::os::unix::fs::symlink("real/plain.nml", dir.join("leaf.nml")).unwrap();
    let root = dir.to_str().unwrap();
    let (code, stdout, stderr) = run(&[
        "binding",
        "--root",
        root,
        dir.join("link/sub/x.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "open + unbound: {stdout}{stderr}");
    assert!(stdout.starts_with("file      real/sub/x.nml\n"), "{stdout}");
    assert!(
        stdout.contains(
            "notes     real/sub/x.nml: info: resolved through a symlink at component 1 of the \
             root-relative path — followed (open universe); a closed binding would reject it \
             (NML2083)"
        ),
        "{stdout}"
    );
    // A link LEAF: the verdict indexes the leaf.
    let (_, stdout, _) = run(&[
        "binding",
        "--root",
        root,
        dir.join("leaf.nml").to_str().unwrap(),
    ]);
    assert!(
        stdout.contains("leaf.nml: info: resolved through a symlink at component 1 of"),
        "{stdout}"
    );
    // A plain path: no such row.
    let (_, stdout, _) = run(&[
        "binding",
        "--root",
        root,
        dir.join("real/plain.nml").to_str().unwrap(),
    ]);
    assert!(!stdout.contains("notes"), "{stdout}");
    // `--json`: the row is a diagnostic in `notes`.
    let (_, stdout, _) = run(&[
        "binding",
        "--json",
        "--root",
        root,
        dir.join("link/sub/x.nml").to_str().unwrap(),
    ]);
    let row = &rows_of(&stdout)[0];
    assert_eq!(row["universe"], "open");
    assert!(
        row["notes"].as_array().unwrap().iter().any(|n| n["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("resolved through a symlink at component 1"))),
        "{stdout}"
    );
    // CLOSED: a manifest at the root closes the universe; the link halts
    // the walk (NML2083) and no `resolved through` row exists.
    std::fs::copy(
        fixture("workspace/demo.package.nml"),
        dir.join("demo.package.nml"),
    )
    .unwrap();
    std::fs::copy(
        fixture("workspace/core.model.nml"),
        dir.join("core.model.nml"),
    )
    .unwrap();
    let (code, stdout, _) = run(&[
        "binding",
        "--root",
        root,
        dir.join("link/sub/x.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(stdout.contains("error[NML2083]"), "{stdout}");
    assert!(!stdout.contains("resolved through"), "{stdout}");
}

/// r69-mem S2: `nml parse` streams its JSON to stdout instead of building
/// the whole document in memory first (70 bytes per input byte on dense
/// input). The bytes are pinned identical to `serde_json::to_string_pretty`
/// of the same AST plus the trailing newline, over every valid fixture.
#[test]
fn parse_streams_bytes_identical_to_the_buffered_form() {
    let root = fixture("valid");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "nml"))
        .collect();
    files.sort();
    assert!(files.len() >= 5, "fixture set: {files:?}");
    for path in files {
        let src = std::fs::read_to_string(&path).unwrap();
        let (file, errors) = nml_core::cst::parse_to_ast_all(&src);
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
        let expected = format!("{}\n", serde_json::to_string_pretty(&file).unwrap());
        let out = nml_bin()
            .args(["parse", path.to_str().unwrap()])
            .output()
            .expect("failed to run nml");
        assert!(out.status.success(), "{}", path.display());
        assert!(
            out.stdout == expected.as_bytes(),
            "{}: streamed bytes differ from the buffered form",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------
// r73-cli claim 3 — leaf safety for the workspace-free verbs.
//
// `parse` and `fmt` have NO universe: an absolute operator path has no
// principled root, and deriving one is not even available (on macOS
// `WorkspaceRoot::derive` refuses `/tmp/x.nml` outright — `/tmp` is a
// link and no `.git` fences the walk). What IS achievable is the leaf's
// own safety: open beneath the leaf's own parent, `O_NOFOLLOW`, and
// `fstat` must say regular file.
// ---------------------------------------------------------------------

/// Pre-fix `nml parse <fifo>` and `nml fmt <fifo>` BLOCKED FOREVER in
/// `read_to_string` (`check`/`validate`/`fix` refuse a FIFO before the
/// open — the operator verbs had no such rule). Both now refuse it at
/// the open, in milliseconds.
#[test]
#[cfg(unix)]
fn workspace_free_verbs_refuse_a_fifo_without_hanging() {
    let dir = scratch_dir("r73-fifo");
    let fifo = dir.join("pipe.nml");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo");
    for verb in ["parse", "fmt"] {
        let started = std::time::Instant::now();
        let (code, stdout, stderr) = run_bounded(&[verb, fifo.to_str().unwrap()], 20);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{verb} hung on a FIFO"
        );
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.contains("`pipe.nml` is not a regular file (refused at open)"),
            "{verb}: {stderr}"
        );
    }
}

/// The same rule refuses a character device: pre-fix `nml parse
/// /dev/zero` read without bound (killed at 5 s having grown past a
/// gigabyte).
#[test]
#[cfg(unix)]
fn a_character_device_is_refused_not_streamed() {
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_bounded(&["parse", "/dev/zero"], 20);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "streamed a device"
    );
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("`zero` is not a regular file (refused at open)"),
        "{stderr}"
    );
}

/// A symlinked LEAF is FOLLOWED by a verb with no universe — the file
/// the operator's link names is the file read and written — and the
/// link SURVIVES the write. Pre-r73 `nml fmt link.nml` wrote a temp
/// beside the link and renamed over it, silently REPLACING the symlink
/// with a regular file; r73-cli refused the link outright, which
/// `check link.nml` (an open universe follows its links by design) and
/// every formatter in the state of the art contradict — `rustfmt` and
/// `gofmt` format the target in place and leave the link (measured, r74
/// decision 1). r74: the leaf is resolved once and the `O_NOFOLLOW`
/// open and the temp-and-rename write are anchored at the TARGET's
/// parent. A dangling link is refused before anything is created, and
/// a link to a FIFO is refused at the open without blocking.
#[test]
#[cfg(unix)]
fn workspace_free_verbs_follow_a_symlinked_leaf_and_leave_it_a_link() {
    let dir = scratch_dir("r74-leaflink");
    let messy = "thing t:\n    a =    1\n";
    std::fs::write(dir.join("real.nml"), messy).unwrap();
    std::fs::write(dir.join("plain.nml"), messy).unwrap();
    std::os::unix::fs::symlink("real.nml", dir.join("link.nml")).unwrap();
    let link = dir.join("link.nml");
    let (code, _, stderr) = run(&["parse", link.to_str().unwrap()]);
    assert_eq!(code, 0, "parse follows the link: {stderr}");
    // The control: the same bytes, named directly.
    let (code, _, stderr) = run(&["fmt", dir.join("plain.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    let expected = std::fs::read_to_string(dir.join("plain.nml")).unwrap();
    assert_ne!(
        expected, messy,
        "the fixture must be something fmt rewrites"
    );
    let (code, stdout, stderr) = run(&["fmt", link.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("formatted"), "{stdout}");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link survived the write"
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("real.nml"),
        "and still points where it did"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("real.nml")).unwrap(),
        expected,
        "the TARGET was formatted"
    );
    // Dangling: refused, nothing created, the link untouched.
    std::os::unix::fs::symlink("absent.nml", dir.join("dangle.nml")).unwrap();
    for verb in ["parse", "fmt"] {
        let (code, stdout, stderr) = run(&[verb, dir.join("dangle.nml").to_str().unwrap()]);
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.contains("`dangle.nml` is a symlink whose target cannot be resolved"),
            "{verb}: {stderr}"
        );
    }
    assert!(
        !dir.join("absent.nml").exists(),
        "nothing was created behind the dangling link"
    );
    assert!(
        std::fs::symlink_metadata(dir.join("dangle.nml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // A link to a FIFO: the resolved leaf is opened `O_NOFOLLOW |
    // O_NONBLOCK` and refused by kind, in milliseconds.
    let fifo = dir.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo");
    std::os::unix::fs::symlink("pipe", dir.join("lp.nml")).unwrap();
    let started = std::time::Instant::now();
    let (code, _, stderr) = run_bounded(&["parse", dir.join("lp.nml").to_str().unwrap()], 20);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "hung on a linked FIFO"
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("`pipe` is not a regular file (refused at open)"),
        "{stderr}"
    );
}

/// An OPEN universe follows a link on the way in (`check link.nml`
/// validates), and pre-r73 its `fix` wrote the repaired text through
/// `write_file_atomically` — which renamed over the LINK: the link
/// became a regular file holding the fix while the real file kept the
/// defect. r73-cli refused the write; r74 makes it land where the read
/// came from: the TARGET holds the fix and the link stands. A closed
/// universe still rejects a linked key before any read (NML2083:
/// `check_symlinked_content_under_closed_binding_2083`, the link
/// matrix) — this is the open-universe contract only.
#[test]
#[cfg(unix)]
fn an_open_universe_fix_writes_through_a_symlink_and_keeps_it() {
    let dir = scratch_dir("r74-fixlink");
    let body = "model m:\n    name string\n\nm X:\n    nme = \"x\"\n";
    std::fs::write(dir.join("real.nml"), body).unwrap();
    std::os::unix::fs::symlink("real.nml", dir.join("l.nml")).unwrap();
    let link = dir.join("l.nml");
    // The control: the same file, named directly.
    std::fs::write(dir.join("c.nml"), body).unwrap();
    let (code, stdout, stderr) = run(&["fix", dir.join("c.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("1 edit(s) applied"), "{stdout}");
    let fixed = std::fs::read_to_string(dir.join("c.nml")).unwrap();
    assert_ne!(fixed, body, "the control was rewritten");

    let (code, stdout, stderr) = run(&["fix", link.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("1 edit(s) applied"), "{stdout}");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link survived"
    );
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("real.nml"));
    assert_eq!(
        std::fs::read_to_string(dir.join("real.nml")).unwrap(),
        fixed,
        "the real file holds the fix"
    );
}

/// The leaf rule does not cost the operator their typed prefix: links
/// ABOVE the leaf are followed exactly as before (`/tmp` is a link on
/// macOS), and a bare file name still resolves against the working
/// directory.
#[test]
fn the_leaf_rule_keeps_a_linked_prefix_and_a_bare_name() {
    let dir = scratch_dir("r73-prefix");
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::fs::write(dir.join("real/x.nml"), "thing t:\n    a = 1\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("real", dir.join("via")).unwrap();
    #[cfg(unix)]
    {
        let through = dir.join("via/x.nml");
        let (code, _, stderr) = run(&["parse", through.to_str().unwrap()]);
        assert_eq!(code, 0, "a linked PREFIX is followed: {stderr}");
    }
    let (code, _, stderr) = run_in(&dir.join("real"), &["parse", "x.nml"]);
    assert_eq!(code, 0, "a bare name resolves against the cwd: {stderr}");
}

// ---------------------------------------------------------------------
// r73-cli claims 1, 4, 5 — the run's ONE closing row, the run-scoped
// universe facts on it, and the per-code finding budget.
// ---------------------------------------------------------------------

/// `(exit code, the parsed NDJSON rows AFTER the contract row)` of a
/// `--json` run — every stream opens with the contract row, asserted
/// byte for byte by [`rows_of`] on every call, so the ratchet rides
/// every `--json` pin in this file.
fn json_rows(args: &[&str]) -> (i32, Vec<serde_json::Value>) {
    let (code, stdout, stderr) = run(args);
    assert!(
        stderr.is_empty(),
        "{args:?}: stderr must stay silent: {stderr}"
    );
    (code, rows_of(&stdout))
}

/// The wire revision the BINARY writes, read from its own contract row:
/// `nml-cli/src/out.rs` REVISION is the one source, and a pin that copies
/// the number is a second one that a bump has to find.
///
/// r103-cov: asserted PRESENT here, once, for every caller. Reading the
/// number from the row under test makes `last["revision"] ==
/// wire_revision()` self-referential: with the field gone from both rows
/// both sides read `null` and the pin passed — deleting the stamp left
/// the whole cargo suite green and only the separate docs job (the
/// schema's `required`) refused it.
fn wire_revision() -> serde_json::Value {
    let revision = serde_json::from_str::<serde_json::Value>(&contract_line())
        .expect("the contract row parses")["revision"]
        .clone();
    assert!(
        revision.is_u64(),
        "the contract row carries no `revision`: {}",
        contract_line()
    );
    revision
}

/// The exact first line of every `--json` stream: the contract row —
/// the format's number, its revision, the binary — spelled as the
/// writer spells it (compact, keys in order). Asked OF the writer
/// (`nml version --json`, row one) rather than spelled here: the
/// revision has one source, `nml-cli/src/out.rs`, and the docs gate holds
/// the schema's `$defs`, the generated shape record and the CHANGELOG
/// ledger to it. A literal here was a second source that every `--json`
/// pin in this file depended on.
fn contract_line() -> String {
    let out = nml_bin()
        .args(["version", "--json"])
        .output()
        .expect("nml version --json");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let row = stdout.lines().next().expect("the contract row").to_string();
    assert!(
        row.contains("\"type\":\"contract\"")
            && row.contains(&format!("\"nmlVersion\":\"{}\"", env!("CARGO_PKG_VERSION"))),
        "row one is the contract row, from this binary: {row}"
    );
    row
}

/// A `--json` stream's rows after its contract row, which is asserted
/// verbatim: a stream that opens with anything else is no stream of
/// this contract.
fn rows_of(stdout: &str) -> Vec<serde_json::Value> {
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some(contract_line().as_str()),
        "every --json stream opens with the contract row: {stdout}"
    );
    lines
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

/// Claim 1: EVERY verb's `--json` stream ends with exactly one `summary`
/// row carrying the run's `exit`, on EVERY path — a clean run, a failing
/// run, a parse error that never reaches a `result` row, an absent
/// target, a usage error. Pre-fold `check --json` on a parse error ended
/// with an `error` row, `binding` ended with a `binding` row carrying no
/// exit at all, and a consumer had to re-encode each verb's 0/1/2
/// mapping to know what the process had done.
#[test]
fn every_verb_ends_with_one_summary_row_carrying_the_exit() {
    let dir = scratch_dir("r73-summary");
    std::fs::write(
        dir.join("ok.nml"),
        "model m:\n    v string\n\nm X:\n    v = \"x\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("bad.nml"), "thing T:\n    v = = 1\n").unwrap();
    let ok = dir.join("ok.nml").display().to_string();
    let bad = dir.join("bad.nml").display().to_string();
    let absent = dir.join("nope.nml").display().to_string();
    std::fs::copy(dir.join("ok.nml"), dir.join("fmt.nml")).unwrap();
    let fmt_target = dir.join("fmt.nml").display().to_string();
    let ws = workspace_copy("r73-summary-ws");
    let root = ws.to_str().unwrap().to_string();
    let governed = ws.join("tenants/cu/plain.flow.nml").display().to_string();

    let cases: Vec<(Vec<&str>, &str, i64)> = vec![
        (vec!["check", "--json", &ok], "check", 0),
        (vec!["check", "--json", &bad], "check", 1),
        (vec!["check", "--json", &absent], "check", 1),
        (vec!["check", "--json", "--bogus", &ok], "check", 2),
        (vec!["validate", "--json", &ok], "validate", 0),
        (vec!["binding", "--json", &ok], "binding", 1),
        (vec!["fix", "--json", &ok], "fix", 0),
        (vec!["fix", "--json", "--dry-run", &ok], "fix", 0),
        // r75: the exit-2 path (`--schema` beside a governed file; the
        // r74-cert-cli mutant that bypassed `finish` there survived the
        // lane), the verbs that gained `--json` (parse, fmt) and the one
        // whose rows were never pinned (limits).
        (
            vec![
                "check", "--json", "--schema", &root, "--root", &root, &governed,
            ],
            "check",
            2,
        ),
        (vec!["parse", "--json", &ok], "parse", 0),
        (vec!["parse", "--json", &bad], "parse", 1),
        (vec!["fmt", "--json", &fmt_target], "fmt", 0),
        (vec!["limits", "--json"], "limits", 0),
    ];
    for (args, verb, exit) in cases {
        let (code, stdout, stderr) = run(&args);
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
        // The stream opens with the contract row (asserted verbatim by
        // `rows_of`) and its closing row carries the same three facts.
        let rows = rows_of(&stdout);
        let last = rows.last().unwrap_or_else(|| panic!("{args:?}: no rows"));
        assert_eq!(last["type"], "summary", "{args:?}: {stdout}");
        assert_eq!(last["formatVersion"], 1, "{args:?}: {last}");
        assert_eq!(last["revision"], wire_revision(), "{args:?}: {last}");
        assert_eq!(
            last["nmlVersion"],
            env!("CARGO_PKG_VERSION"),
            "{args:?}: {last}"
        );
        assert_eq!(
            rows.iter().filter(|r| r["type"] == "contract").count(),
            0,
            "{args:?}: the contract row is stated once, first"
        );
        assert_eq!(last["verb"], verb, "{args:?}");
        assert_eq!(last["exit"].as_i64(), Some(exit), "{args:?}: {stdout}");
        assert_eq!(
            last["exit"].as_i64(),
            Some(i64::from(code)),
            "{args:?}: the row's exit IS the process exit"
        );
        assert_eq!(
            rows.iter().filter(|r| r["type"] == "summary").count(),
            1,
            "{args:?}: exactly one summary row"
        );
    }
}

/// The stream describes itself before it says anything else: the FIRST
/// line of every `--json` run is the contract row — byte for byte, on
/// every path that emits a row at all: a finding, an answer, a usage
/// error, a non-UTF-8 argument (refused before any verb runs), `-q`
/// (the row is the contract, not a courtesy) and `NML_UNICODE=0` (no
/// prose to fold) — and the closing row repeats its three facts. (A
/// consumer that closes the pipe right after the header is pinned on
/// the EPIPE test below; a stdout closed BEFORE any write is
/// unobservable — Rust's stdout swallows EBADF by design.)
#[test]
fn every_json_stream_opens_with_the_contract_row_on_every_path() {
    let dir = scratch_dir("r96-contract");
    std::fs::write(dir.join("bad.nml"), "thing T:\n    v = = 1\n").unwrap();
    let bad = dir.join("bad.nml").display().to_string();
    let header = contract_line();
    let cases: Vec<Vec<&str>> = vec![
        vec!["version", "--json"],
        vec!["limits", "--json"],
        vec!["explain", "--json", "NML2064"],
        vec!["check", "--json", &bad],
        vec!["check", "--json", "--quiet", &bad],
        vec!["check", "--json", "--no-such-flag"],
        vec!["binding", "--json"],
        vec!["fix", "--json", "--dry-run", &bad],
        vec!["parse", "--json", &bad],
        vec!["fmt", "--json", &bad],
    ];
    for args in cases {
        for unicode in [Some("1"), Some("0")] {
            let (_, stdout, stderr) = run_env(&[("NML_UNICODE", unicode)], &args);
            assert!(stderr.is_empty(), "{args:?}: {stderr}");
            let mut lines = stdout.lines();
            assert_eq!(lines.next(), Some(header.as_str()), "{args:?}: {stdout}");
            let last: serde_json::Value =
                serde_json::from_str(lines.last().unwrap_or_else(|| panic!("{args:?}: {stdout}")))
                    .unwrap();
            assert_eq!(last["type"], "summary", "{args:?}: {last}");
            assert_eq!(last["formatVersion"], 1, "{args:?}: {last}");
            assert_eq!(last["revision"], wire_revision(), "{args:?}: {last}");
            assert_eq!(
                last["nmlVersion"],
                env!("CARGO_PKG_VERSION"),
                "{args:?}: {last}"
            );
        }
    }
    // A `--help` page is output, not a run: no row, so no contract row.
    let (code, stdout, _) = run(&["check", "--json", "--help"]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("\"contract\""), "{stdout}");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        use std::process::{Command, Stdio};
        // An argument that is not UTF-8 is refused before any verb runs;
        // the refusal is a row, and the contract row comes first.
        let out = Command::new(env!("CARGO_BIN_EXE_nml"))
            .args(["check", "--json"])
            .arg(std::ffi::OsStr::from_bytes(b"\xff.nml"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(stdout.lines().next(), Some(header.as_str()), "{stdout}");
        let rows = rows_of(&stdout);
        assert_eq!(rows[0]["type"], "error", "{stdout}");
        assert_eq!(rows[0]["kind"], "usage", "{stdout}");
    }
}

/// Claim 4: the run-scoped universe facts — the root, HOW it was fixed
/// (`RootOrigin::label`), whether the universe is closed and how many
/// manifests it discovered — ride the closing row, so a `check`
/// consumer learns them without running `nml binding` as a second
/// process. They are stated ONCE for the run, not repeated per target.
#[test]
fn the_closing_row_carries_the_runs_universe_facts() {
    let dir = workspace_copy("r73-universe");
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (_, r) = json_rows(&["check", "--json", "--root", &root, &plain]);
    let last = r.last().unwrap();
    assert_eq!(last["universe"], "closed");
    assert_eq!(last["root"]["origin"], "explicit");
    assert!(last["manifests"].as_u64().unwrap() >= 1, "{last}");
    // r74: the budget units the walk stopped inside (r73-kernel) ride
    // the same row. None was spent here, so the list is present and
    // EMPTY — never absent, so a consumer can tell "no unit was denied"
    // from "this build does not report units". The spent case is pinned
    // at the real bound in
    // `perf_entry_bound_truncation_denies_only_the_tenant_unit`.
    assert_eq!(last["truncatedUnits"], serde_json::json!([]), "{last}");
    assert_eq!(last["targets"], 1);
    assert_eq!(
        r.iter().filter(|row| row["type"] == "summary").count(),
        1,
        "stated once for the run"
    );

    // A derived root names the fence it was derived from: the fact the
    // no-VCS narrowing needed a diagnostic code to convey before. Each
    // origin is pinned where it is deterministic. A `.git` entry of ANY
    // kind in the target's directory fences the walk there (a linked
    // worktree carries a `.git` FILE): "vcs-fence", the root that
    // directory — hermetic wherever the scratch directory sits.
    let fenced = scratch_dir("r73-fenced");
    std::fs::write(fenced.join(".git"), "gitdir: /nonexistent\n").unwrap();
    std::fs::write(fenced.join("a.nml"), "thing t:\n    a = 1\n").unwrap();
    let (_, r) = json_rows(&["check", "--json", fenced.join("a.nml").to_str().unwrap()]);
    let last = r.last().unwrap();
    assert_eq!(last["universe"], "open", "{last}");
    assert_eq!(last["root"]["origin"], "derivedVcsFence", "{last}");
    assert_eq!(
        last["root"]["path"],
        fenced.canonicalize().unwrap().to_str().unwrap(),
        "{last}"
    );
    assert_eq!(last["truncatedUnits"], serde_json::json!([]), "{last}");
    // No fence at all — only a directory outside the checkout can show
    // it: the universe narrows to the target's own directory.
    let bare = unfenced_temp_dir("r73-bare");
    std::fs::write(bare.join("a.nml"), "thing t:\n    a = 1\n").unwrap();
    let (_, r) = json_rows(&["check", "--json", bare.join("a.nml").to_str().unwrap()]);
    let last = r.last().unwrap();
    assert_eq!(last["universe"], "open", "{last}");
    assert_eq!(last["root"]["origin"], "derivedTargetDir", "{last}");
    assert_eq!(last["root"]["path"], bare.to_str().unwrap(), "{last}");
    assert_eq!(last["truncatedUnits"], serde_json::json!([]), "{last}");
}

/// A derivation that met no `.git` fence at all narrows the universe to
/// the target's own directory (`derivedTargetDir`) — a manifest above it
/// is never seen: exactly the non-obvious root the `note: workspace root
/// …` disclosure exists for — so the CLI says so on stderr in human mode,
/// as it does for a `.git` FILE fence and a shadow; `-q` keeps it silent;
/// `--json` carries it on the root object as before. Only a directory
/// outside every checkout can show it.
#[test]
fn a_no_fence_derivation_is_disclosed_on_stderr() {
    let bare = unfenced_temp_dir("no-fence-note");
    std::fs::write(bare.join("a.nml"), "thing t:\n    a = 1\n").unwrap();
    let target = bare.join("a.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", &target]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    assert_eq!(
        stderr,
        format!(
            "note: workspace root {}  (derived: no .git fence found, so the target's own \
             directory is the workspace root — pass --root to pin)\n",
            bare.display()
        ),
        "{stderr}"
    );
    let (code, _, stderr) = run(&["check", "-q", &target]);
    assert_eq!(code, 0);
    assert_eq!(stderr, "", "{stderr}");
    let (_, r) = json_rows(&["check", "--json", &target]);
    let last = r.last().unwrap();
    assert_eq!(last["root"]["origin"], "derivedTargetDir", "{last}");
}

/// Colour paints ONLY the severity prefix — `error[NML2008]`,
/// `warning[…]`, `note:`, `help:`, the closing `error:` — in rustc's
/// palette, on stderr, and only when `CLICOLOR_FORCE` says so or stderr
/// is a terminal: never under a pipe (so every other pin's bytes are
/// what they were), `NO_COLOR` wins over the force (no-color.org), and
/// `--json` rows and a `binding` block on stdout never carry it. The
/// piped default is the coloured run with the SGR wraps removed, byte
/// for byte.
#[test]
fn colour_paints_the_severity_prefix_only_when_forced_or_on_a_tty() {
    let dir = workspace_copy("colour");
    let root = dir.to_str().unwrap();
    let bad = dir.join("tenants/cu/bad.flow.nml").display().to_string();
    let grant = dir
        .join("tenants/cu/member-lookup.flow.nml")
        .display()
        .to_string();
    let off = [
        ("NO_COLOR", None),
        ("CLICOLOR_FORCE", None),
        ("TERM", Some("xterm")),
    ];
    let on = [
        ("NO_COLOR", None),
        ("CLICOLOR_FORCE", Some("1")),
        ("TERM", Some("xterm")),
    ];
    let (code, _, plain) = run_env(&off, &["check", "--root", root, &bad, &grant]);
    assert_eq!(code, 1, "{plain}");
    assert!(!plain.contains('\x1b'), "a pipe is never coloured: {plain}");
    let (code, stdout, painted) = run_env(&on, &["check", "--root", root, &bad, &grant]);
    assert_eq!(code, 1, "{painted}");
    for wrapped in [
        "\x1b[1;31merror[NML2008]\x1b[0m",
        "\x1b[1;33mwarning[NML2080]\x1b[0m",
        "\x1b[1;36mhelp:\x1b[0m the block to add after line",
        "\x1b[1;31merror:\x1b[0m 2 of 2 file(s) failed",
    ] {
        assert!(painted.contains(wrapped), "{wrapped:?} in {painted}");
    }
    assert!(
        !stdout.contains('\x1b'),
        "stdout is never coloured: {stdout}"
    );
    let stripped = painted
        .replace("\x1b[1;31m", "")
        .replace("\x1b[1;33m", "")
        .replace("\x1b[1;32m", "")
        .replace("\x1b[1;36m", "")
        .replace("\x1b[0m", "");
    assert_eq!(stripped, plain, "the prefix's wrap alone differs");
    // `NO_COLOR` wins over the force; `CLICOLOR_FORCE=0` is no force.
    for vars in [
        [
            ("NO_COLOR", Some("1")),
            ("CLICOLOR_FORCE", Some("1")),
            ("TERM", Some("xterm")),
        ],
        [
            ("NO_COLOR", None),
            ("CLICOLOR_FORCE", Some("0")),
            ("TERM", Some("xterm")),
        ],
    ] {
        let (_, _, stderr) = run_env(&vars, &["check", "--root", root, &bad]);
        assert!(!stderr.contains('\x1b'), "{vars:?}: {stderr}");
    }
    // A `note:` line and `--json` under the force.
    let (_, _, stderr) = run_env(&on, &["check", "--max-findings", "1", "--root", root, &bad]);
    assert!(
        stderr.contains("\x1b[1;32mnote:\x1b[0m 2 more finding(s) not shown"),
        "{stderr}"
    );
    let (_, stdout, stderr) = run_env(&on, &["check", "--json", "--root", root, &bad]);
    assert!(
        !stdout.contains('\x1b') && stderr.is_empty(),
        "{stdout}{stderr}"
    );
    let (_, stdout, _) = run_env(&on, &["binding", "--root", root, &bad]);
    // The block goes through the sanitizer, so a painted prefix would show
    // as `\u{1b}[…` TEXT, never a raw byte: the prefix must be spelled plain.
    assert!(
        stdout.contains("notes     tenants/cu/nml-project.nml: warning[NML2080]:")
            && !stdout.contains('\x1b')
            && !stdout.contains("\\u{1b}"),
        "a block is an answer, never painted: {stdout}"
    );
}

/// Claim 5c/5d: the reporting budget is on BY DEFAULT (clang ships
/// `-ferror-limit=20`), the counts and the exit code stay EXACT, the
/// truncation is disclosed in both surfaces, `--max-findings 0` restores
/// the full stream byte-for-byte, and a code that floods from line 6
/// cannot crowd out a rarer code arriving at the end of the file.
#[test]
fn the_finding_budget_is_fair_exact_and_liftable() {
    let dir = scratch_dir("r73-budget");
    let mut src = String::from("model m:\n    keep string\n\nm X:\n    keep = \"x\"\n");
    for i in 0..20_000 {
        src.push_str(&format!("    p{i:05} = 1\n"));
    }
    // The rare code arrives LAST, long past the flooding code's share.
    src.push_str("\nm Y:\n    keep = 7\n");
    let file = dir.join("flood.nml");
    std::fs::write(&file, &src).unwrap();
    let file = file.display().to_string();

    let (code, _, stderr) = run(&["check", "--strict", &file]);
    assert_eq!(code, 1);
    // The count is exact whatever was printed.
    assert!(stderr.contains("error: 20001 error(s)"), "{stderr}");
    assert!(
        stderr.contains("note: 19552 more finding(s) not shown (limit 512; NML2001 \u{d7}19552)"),
        "the truncation is disclosed with the exact per-code count: {stderr}"
    );
    assert!(
        stderr.contains("--max-findings 0"),
        "the trailer names the escape hatch: {stderr}"
    );
    assert!(
        stderr.contains("NML2008"),
        "the rare code at the END of the file survived the flood: {stderr}"
    );
    let shown = stderr.lines().filter(|l| l.contains("NML2001:")).count()
        + stderr
            .lines()
            .filter(|l| l.contains("error[NML2001]"))
            .count();
    assert!(shown <= 512, "the budget bounds what is printed: {shown}");

    // `--max-findings 0` restores every line.
    let (code, _, full) = run(&["check", "--strict", "--max-findings", "0", &file]);
    assert_eq!(code, 1);
    assert!(
        full.matches("error[NML2001]").count() == 20_000,
        "uncapped: {}",
        full.matches("error[NML2001]").count()
    );
    assert!(!full.contains("more finding(s) not shown"), "no trailer");

    // `--json` carries the same facts as data.
    let (code, r) = json_rows(&["check", "--strict", "--json", &file]);
    assert_eq!(code, 1);
    let last = r.last().unwrap();
    assert_eq!(last["errors"], 20001);
    assert_eq!(last["withheld"]["hidden"], 19552);
    assert_eq!(last["withheld"]["byCode"]["NML2001"], 19552);
    // 448 of the flooding code (the budget less the reserve held back
    // for codes not yet seen) plus the one rare code that arrived last.
    assert_eq!(last["withheld"]["shown"], 449);
    assert!(
        r.iter()
            .any(|row| row["code"] == "NML2008" && row["type"] == "diagnostic"),
        "the rare code is a row too"
    );

    // r75 (r74-cert-cli F2): fairness is ORDER-INSENSITIVE. A rare code
    // arriving FIRST stayed "starved" for the rest of the run (its one
    // finding is below its 64-line share), and the old rule refused the
    // later flood at 64 lines — 65 shown of 512. The rule now refuses a
    // past-share code only when what is left of the budget could not
    // still give every starved code its share AND keep one share for a
    // code not yet seen: 383 of the flood plus the rare one = 384.
    let mut rare_first =
        String::from("model m:\n    keep string\n\nm Y:\n    keep = 7\n\nm X:\n    keep = \"x\"\n");
    for i in 0..20_000 {
        rare_first.push_str(&format!("    p{i:05} = 1\n"));
    }
    let rare = dir.join("rare-first.nml");
    std::fs::write(&rare, &rare_first).unwrap();
    let rare = rare.display().to_string();
    let (code, r) = json_rows(&["check", "--strict", "--json", &rare]);
    assert_eq!(code, 1);
    let last = r.last().unwrap();
    assert_eq!(last["errors"], 20001, "{last}");
    assert_eq!(last["withheld"]["shown"], 384, "{last}");
    assert_eq!(last["withheld"]["hidden"], 19617, "{last}");
    assert_eq!(
        r.iter().filter(|row| row["code"] == "NML2001").count(),
        383,
        "the flood is capped by the reserve, not by the rare code's share"
    );

    // The TOTAL budget is the only rule that can deny a code seen for
    // the FIRST time (the per-code rules only bind a code already past
    // its share): four distinct codes under a budget of two must print
    // two findings, not four.
    let three = dir.join("three.nml");
    std::fs::write(
        &three,
        "model m:\n    name string\n\nm A:\n    name = 1\n    bogus = 2\n\nm B:\n    \
         other = \"x\"\n\nm C:\n    name = @nope\n",
    )
    .unwrap();
    let three = three.display().to_string();
    let (_, r) = json_rows(&["check", "--strict", "--json", "--max-findings", "2", &three]);
    let last = r.last().unwrap();
    assert_eq!(
        last["withheld"]["shown"], 2,
        "the total budget bounds the run"
    );
    assert_eq!(
        r.iter().filter(|row| row["type"] == "diagnostic").count(),
        2,
        "exactly the budget's worth of rows: {r:#?}"
    );
    assert!(
        last["withheld"]["byCode"]
            .as_object()
            .unwrap()
            .keys()
            .count()
            >= 2,
        "more codes than the budget could print: {last}"
    );

    // A bad count is a usage error (exit 2), not a silent default.
    let (code, _, stderr) = run(&["check", "--max-findings", "x", &file]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--max-findings takes a count"), "{stderr}");
}

/// Claim 5a: stderr is BUFFERED when it is not a terminal, so a flood
/// costs one `write(2)` per 256 KiB instead of ~7 per finding — and the
/// bytes are byte-identical to the unbuffered stream. Pinned as an
/// ordering contract: the two streams still interleave per target, so a
/// `2>&1` capture reads exactly as it did.
#[test]
fn buffered_stderr_keeps_the_per_target_interleaving() {
    let dir = scratch_dir("r73-interleave");
    std::fs::write(
        dir.join("bad.nml"),
        "model m:\n    v string\n\nm X:\n    v = 1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("good.nml"),
        "model m2:\n    v string\n\nm2 Y:\n    v = \"x\"\n",
    )
    .unwrap();
    // One process, both streams into one pipe: the failing target's
    // diagnostic must still precede the clean target's stdout verdict.
    let out = nml_bin()
        .args([
            "check",
            dir.join("bad.nml").to_str().unwrap(),
            dir.join("good.nml").to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stderr.contains("NML2008"), "{stderr}");
    assert!(stdout.contains("good.nml: ok"), "{stdout}");
    assert_eq!(out.status.code(), Some(1));
}

// ---------------------------------------------------------------------
// r73-cli claim 2 — `nml fix --check`, the CI gate.
// ---------------------------------------------------------------------

/// `--check` is the formatter idiom (rustfmt, black, prettier, gofmt):
/// write nothing, exit 1 if any edit WOULD apply. `--dry-run` keeps the
/// Unix idiom — "show me what would happen", exit 0 — because a script
/// piping a dry run into review tooling under `set -e` must not begin
/// failing the day its tree grows a fixable finding. `--check` implies
/// `--dry-run`, so the diff prints and there is one flag to remember.
#[test]
fn fix_check_is_a_gate_and_dry_run_is_not() {
    let dir = scratch_dir("r73-fixcheck");
    let body = "model m:\n    name string\n\nm X:\n    nme = \"x\"\n";
    std::fs::write(dir.join("a.nml"), body).unwrap();
    let a = dir.join("a.nml").display().to_string();

    // Pending edits: `--check` fails, writes nothing, prints the diff.
    let (code, stdout, stderr) = run(&["fix", "--check", &a]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("-    nme = \"x\""),
        "the diff prints: {stdout}"
    );
    assert!(
        stderr.contains("1 fix(es) would apply"),
        "the failure names the count: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nml")).unwrap(),
        body,
        "nothing was written"
    );

    // The same run as `--dry-run` reports the same edits and exits 0.
    let (code, dry, _) = run(&["fix", "--dry-run", &a]);
    assert_eq!(code, 0, "a dry run is an inspection, not a gate");
    assert!(dry.contains("-    nme = \"x\""), "{dry}");

    // `--json`: the closing row's own `exit` carries the gate's verdict.
    let (code, r) = json_rows(&["fix", "--check", "--json", &a]);
    assert_eq!(code, 1);
    let last = r.last().unwrap();
    assert_eq!(last["exit"], 1);
    assert_eq!(last["edits"], 1);
    assert_eq!(last["dryRun"], true);

    // A clean tree passes the gate.
    let (code, _, stderr) = run(&["fix", &a]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = run(&["fix", "--check", &a]);
    assert_eq!(code, 0, "a fixpoint passes the gate: {stderr}");
}

/// r80-cov (mutant F5 survived): a standing WARNING is counted in
/// `remaining` ("diagnostic(s) not auto-fixable", and the closing row's
/// `remaining`) — and `--check` does not gate on it: only an error no fix
/// repairs, or a pending edit, fails the gate.
#[test]
fn fix_counts_a_standing_warning_as_remaining_and_the_gate_ignores_it() {
    let dir = scratch_dir("r80-warning-remaining");
    std::fs::write(dir.join("m.model.nml"), "model m:\n    name string\n").unwrap();
    let body = "m a:\n    name = \"x\"\n    extra = 1\n";
    std::fs::write(dir.join("w.nml"), body).unwrap();
    let schema = dir.to_str().unwrap().to_string();
    let w = dir.join("w.nml").display().to_string();

    let (code, stdout, stderr) = run(&["fix", "--schema", &schema, &w]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout
            .contains("0 edit(s) applied across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable"),
        "the warning is a standing diagnostic: {stdout}"
    );
    assert_eq!(std::fs::read_to_string(dir.join("w.nml")).unwrap(), body);

    let (code, stdout, stderr) = run(&["fix", "--check", "--schema", &schema, &w]);
    assert_eq!(code, 0, "a warning never fails the gate: {stdout}{stderr}");
    assert!(
        stdout.contains("1 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
    assert!(!stderr.contains("remain that no fix repairs"), "{stderr}");

    let (code, r) = json_rows(&["fix", "--check", "--json", "--schema", &schema, &w]);
    assert_eq!(code, 0);
    let last = r.last().unwrap();
    assert_eq!(last["remaining"], 1, "{last}");
    assert_eq!(last["errors"], 0, "{last}");
    assert_eq!(last["exit"], 0, "{last}");
}

// ---------------------------------------------------------------------
// r73-cli claim 6 — `nml limits`.
// ---------------------------------------------------------------------

/// The bounds are a first-class surface, not a number you learn from an
/// error message: `nml limits` publishes each with what it bounds and
/// WHO can reach it, and `--json` carries the same as data, including
/// the bounds the table deliberately does not publish and why.
#[test]
fn limits_publishes_the_bounds_and_their_reach() {
    let (code, stdout, stderr) = run(&["limits"]);
    assert_eq!(code, 0, "{stderr}");
    // r75: the three-axis taxonomy — reach × guards × surface. The
    // `operator` class is empty in this tree (every bound sits behind a
    // file the operator points the tool at), so the human table groups
    // `[content]` and `[peer]`.
    assert!(
        stdout.contains("[content]") && stdout.contains("[peer]"),
        "{stdout}"
    );
    assert!(!stdout.contains("[tenant]"), "{stdout}");
    // The two names that collide across modules are QUALIFIED, so the
    // table never reads as a contradiction.
    assert!(
        stdout.contains("nml-core::cst::parser::MAX_DEPTH"),
        "{stdout}"
    );
    assert!(stdout.contains("nml-core::diff::MAX_DEPTH"), "{stdout}");
    assert!(
        stdout.contains("nml-cli::workspace::MAX_TARGET_BYTES"),
        "{stdout}"
    );
    // The per-kind input caps are census rows — the crate's own
    // `MAX_MANIFEST_BYTES` / `MAX_SOURCE_BYTES`, declared in the
    // filesystem leaf that enforces them (every layer that reads names
    // them from there) — shown as every neighbouring byte bound is.
    assert!(
        stdout.contains("nml-validate::fs::MAX_MANIFEST_BYTES")
            && stdout.contains("nml-validate::fs::MAX_SOURCE_BYTES")
            && stdout.contains("256 KiB")
            && stdout.contains("4 MiB"),
        "{stdout}"
    );
    // The two bounds this round added: the store pointer and the
    // editor transport's HEADER line (the body's bound was no
    // protection while the headers were read unbounded).
    assert!(
        stdout.contains("nml-validate::store::MAX_POINTER_BYTES")
            && stdout.contains("nml-lsp::MAX_HEADER_BYTES"),
        "{stdout}"
    );

    let (code, r) = json_rows(&["limits", "--json"]);
    assert_eq!(code, 0);
    assert!(r.len() > 30, "{} rows", r.len());
    // Every row but the run's closing `summary` (claim 1) is a limit.
    assert_eq!(r.last().unwrap()["type"], "summary");
    assert_eq!(r.last().unwrap()["verb"], "limits");
    let r = &r[..r.len() - 1];
    assert!(r.iter().all(|row| row["type"] == "limit"));
    let stack = r
        .iter()
        .find(|row| row["name"] == "nml-core::layers::MAX_STACK_DEPTH")
        .expect("the uses-stack bound");
    assert_eq!(stack["value"], "16");
    assert_eq!(stack["reach"], "content");
    assert_eq!(stack["guards"], "work");
    assert_eq!(stack["surface"], "kernel");
    assert_eq!(stack["published"], true);
    // The editor bounds a tenant reaches are published with their real
    // reach.
    let index = r
        .iter()
        .find(|row| row["name"] == "nml-lsp::server::MAX_INDEX_BYTES")
        .expect("the index's per-file byte bound is published");
    assert_eq!(index["reach"], "content");
    assert_eq!(index["surface"], "editor");
    let frame = r
        .iter()
        .find(|row| row["name"] == "nml-lsp::MAX_FRAME_BYTES")
        .expect("the frame bound is published");
    assert_eq!(frame["reach"], "peer");
    assert!(
        r.iter()
            .any(|row| row["reach"] == "internal" && !row["what"].as_str().unwrap().is_empty()),
        "an unpublished bound is disclosed WITH its reason"
    );

    // The verb is discoverable and its help is stdout/exit 0.
    let (code, help, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(help.contains("    limits "), "{help}");
    let (code, stdout, stderr) = run(&["limits", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("usage: nml limits"), "{stdout}");
    // A near-miss reaches it through the same did-you-mean every verb has.
    let (code, _, stderr) = run(&["limit"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("did you mean `limits`"), "{stderr}");
}

// ---------------------------------------------------------------------
// r75 — the r74 certification folds on the CLI: `fix --check` a true
// gate (r74-cert-cli F1), help pages under `--json` (F5), `binding`'s
// counters (F7), the row's `closure` (F8), the NDJSON contract of the
// hidden verb and the workspace-free verbs (r73 decision 9), and the
// byte-budget sentence with no `--root` advice (r74-kernel F7).
// ---------------------------------------------------------------------

/// r75 (r74-cert-cli F1): `nml fix --check` is a TRUE CI gate. It used
/// to fail only on a pending edit, so a file that does not parse, a
/// symlink a tenant planted under a closed binding and a unit-denied
/// file all passed it with exit 0 — the finding on stderr of a job that
/// passes. Now an error-severity finding no fix repairs fails the gate,
/// as rustfmt and prettier fail `--check` on a file they cannot parse.
/// ONLY the gate: `fix --dry-run` and a plain `fix` keep the fixer's
/// exit (a refused path counts as not auto-fixable, exit 0 — the r64
/// decision), and a fixpoint still passes.
#[cfg(unix)]
#[test]
fn fix_check_fails_on_an_error_no_fix_repairs() {
    let dir = scratch_dir("r75-gate");
    std::fs::write(dir.join("bad.nml"), "thing T:\n    v = = 1\n").unwrap();
    let bad = dir.join("bad.nml").display().to_string();
    // A file that does not parse.
    let (code, stdout, stderr) = run(&["fix", "--check", &bad]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error(s) remain that no fix repairs"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was written"), "{stderr}");
    assert!(!stderr.contains("would apply"), "{stderr}");
    let (code, r) = json_rows(&["fix", "--check", "--json", &bad]);
    assert_eq!(code, 1);
    let last = r.last().unwrap();
    assert_eq!(last["exit"], 1, "{last}");
    assert_eq!(last["edits"], 0, "{last}");
    // `fix` REPORTS none of a file's standing findings (it counts them
    // as `remaining`; `errors` counts what a run printed), so the gate's
    // reason is `remaining` and its verdict is `exit`.
    assert!(last["remaining"].as_u64().unwrap() >= 1, "{last}");
    // ONLY under `--check`: the dry run and the plain run keep exit 0.
    let (code, _, stderr) = run(&["fix", "--dry-run", &bad]);
    assert_eq!(code, 0, "a dry run is an inspection, not a gate: {stderr}");
    let (code, _, stderr) = run(&["fix", &bad]);
    assert_eq!(
        code, 0,
        "the fixer's own exit stands without the gate: {stderr}"
    );

    // A symlink a tenant planted under a closed binding: NML2083 before
    // any read, the gate raised, nothing written, the link intact.
    let ws = workspace_copy("r75-gate-planted");
    let body = "model m:\n    name string\n\nm X:\n    nme = \"x\"\n";
    std::fs::write(ws.join("vendor/planted.flow.nml"), body).unwrap();
    std::os::unix::fs::symlink(
        "../../vendor/planted.flow.nml",
        ws.join("tenants/cu/planted.flow.nml"),
    )
    .unwrap();
    let root = ws.to_str().unwrap();
    let planted = ws.join("tenants/cu/planted.flow.nml");
    let (code, stdout, stderr) =
        run(&["fix", "--check", "--root", root, planted.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains("error[NML2083]"), "{stderr}");
    // A refused PATH could not be fixed — under the gate and without it
    // alike (exit 1, as an absent path): the gate's own sentence is for
    // the edits and errors of files the fixer could open.
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&["fix", "--root", root, planted.to_str().unwrap()]);
    assert_eq!(
        code, 1,
        "the plain run: a refused path could not be fixed: {stderr}"
    );
    assert!(
        std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("vendor/planted.flow.nml")).unwrap(),
        body,
        "nothing was written through the link"
    );
    // A pending edit AND an error: both are named.
    std::fs::write(dir.join("fixme.nml"), body).unwrap();
    let fixme = dir.join("fixme.nml").display().to_string();
    let (code, _, stderr) = run(&["fix", "--check", &fixme, &bad]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("1 fix(es) would apply"), "{stderr}");
    assert!(
        stderr.contains("error(s) remain that no fix repairs"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("fixme.nml")).unwrap(),
        body
    );
}

/// r75 (r74-cert-cli F5): `--help` is output, not a run — under `--json`
/// it is the page ALONE, exit 0, no closing `summary` row (the one path
/// that mixed human text into the NDJSON stream), for every verb that
/// takes `--json`.
#[test]
fn a_help_page_under_json_is_the_page_alone() {
    for verb in [
        "check", "validate", "fix", "binding", "limits", "parse", "fmt",
    ] {
        let (code, stdout, stderr) = run(&[verb, "--json", "--help"]);
        assert_eq!(code, 0, "{verb}: {stderr}");
        assert!(
            stdout.starts_with(&format!("usage: nml {verb}")),
            "{verb}: {stdout}"
        );
        assert!(
            !stdout.lines().any(|l| l.starts_with('{')),
            "{verb}: a JSON row rode the help page: {stdout}"
        );
        assert!(stderr.is_empty(), "{verb}: {stderr}");
    }
}

/// r75 (r73 decision 9, r74-cert-cli's pre-existing list): the NDJSON
/// contract holds for the workspace-free verbs. `parse --json` emits
/// one `parse` row carrying the AST and `fmt --json` one `fmt` row
/// saying whether the rewrite changed the bytes (both read `--json` as
/// a file name before); an unknown flag is rejected as in every verb.
#[test]
fn the_workspace_free_verbs_speak_ndjson() {
    let valid = "tests/fixtures/valid/minimal-service.nml";
    let (code, rows) = json_rows(&["parse", "--json", valid]);
    assert_eq!(code, 0);
    let parse = rows
        .iter()
        .find(|r| r["type"] == "parse")
        .expect("a parse row");
    assert_eq!(parse["file"], valid);
    assert!(parse["ast"]["declarations"].is_array(), "{parse}");
    assert_eq!(rows.last().unwrap()["type"], "summary");
    assert_eq!(rows.last().unwrap()["exit"], 0);

    let dir = scratch_dir("r75-fmt-json");
    std::fs::write(dir.join("messy.nml"), "thing t:\n    a =    1\n").unwrap();
    let messy = dir.join("messy.nml").display().to_string();
    let (code, rows) = json_rows(&["fmt", "--json", &messy]);
    assert_eq!(code, 0);
    let fmt = rows.iter().find(|r| r["type"] == "fmt").expect("a fmt row");
    assert_eq!(fmt["file"], messy);
    assert_eq!(fmt["changed"], true, "{fmt}");
    let (_, rows) = json_rows(&["fmt", "--json", &messy]);
    let fmt = rows.iter().find(|r| r["type"] == "fmt").expect("a fmt row");
    assert_eq!(fmt["changed"], false, "a fixpoint: {fmt}");

    for (verb, usage) in [
        ("parse", "usage: nml parse [--json] [--quiet] <file>"),
        (
            "fmt",
            "usage: nml fmt [--root <dir>] [--dry-run] [--check] [--json] [--quiet] <path>...",
        ),
    ] {
        let (code, _, stderr) = run(&[verb, "--bogus", valid]);
        assert_eq!(code, 2, "{verb}: a usage error: {stderr}");
        assert!(
            stderr.contains(&format!("unknown flag --bogus; {usage}")),
            "{verb}: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------
// `nml fmt` in the shape of its siblings (r105-ux P1): `<path>...`, the
// CI gate, the dry run, `-q`, one `fmt` row per file, and its two
// reaches — a bare file at its leaf, a tree through the door.
// ---------------------------------------------------------------------

/// `--check` is the formatter's CI gate, spelled as rustfmt, black,
/// prettier and gofmt spell it: nothing written, the diff printed, exit
/// 1 on a file not in canonical style and 0 once the tree is canonical;
/// `--dry-run` shows the same diff and exits 0. A set run closes with one
/// tally; a single file's own line is its verdict. `-q` keeps the diff
/// (the verb's answer) and the gate's error, and nothing else.
#[test]
fn fmt_check_is_the_ci_gate_and_dry_run_writes_nothing() {
    let dir = scratch_dir("r105-fmt-gate");
    let messy = "thing t:\n    a =    1\n";
    std::fs::write(dir.join("a.nml"), messy).unwrap();
    std::fs::write(dir.join("b.nml"), "thing u:\n    b = 2\n").unwrap();
    let root = dir.to_str().unwrap().to_string();
    let a = dir.join("a.nml").display().to_string();

    // The gate, over the tree: diff + tally + the gate's sentence, exit 1.
    let (code, stdout, stderr) = run(&["fmt", "--check", "--root", &root, &root]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("-    a =    1\n+    a = 1\n"),
        "the diff: {stdout}"
    );
    assert!(
        stdout.contains("1 of 2 file(s) not in canonical style\n"),
        "{stdout}"
    );
    assert!(
        stderr.contains(
            "error: 1 file(s) not in canonical style — run `nml fmt` to format them; nothing was written"
        ),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nml")).unwrap(),
        messy,
        "nothing was written"
    );

    // The dry run: the same diff, exit 0, nothing written.
    let (code, stdout, _) = run(&["fmt", "--dry-run", &a]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("-    a =    1\n+    a = 1\nwould format "),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(dir.join("a.nml")).unwrap(), messy);

    // `-q`: the diff and the error stay, the success lines and tally go.
    let (code, stdout, stderr) = run(&["fmt", "-q", "--check", "--root", &root, &root]);
    assert_eq!(code, 1);
    assert!(stdout.contains("+    a = 1\n"), "{stdout}");
    assert!(
        !stdout.contains("would format") && !stdout.contains("file(s)"),
        "{stdout}"
    );
    assert!(
        stderr.starts_with("error: 1 file(s) not in canonical style"),
        "{stderr}"
    );

    // The real run rewrites, then the gate is green and the tree canonical.
    let (code, stdout, _) = run(&["fmt", "--root", &root, &root]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("formatted ") && stdout.contains("formatted 1 of 2 file(s)\n"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nml")).unwrap(),
        "thing t:\n    a = 1\n"
    );
    let (code, stdout, stderr) = run(&["fmt", "--check", "--root", &root, &root]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("0 of 2 file(s) not in canonical style\n"),
        "{stdout}"
    );
    // A single canonical file: its own line is the verdict; silent under -q.
    let (_, stdout, _) = run(&["fmt", &a]);
    assert!(
        stdout.ends_with(": already in canonical style\n"),
        "{stdout}"
    );
    let (code, stdout, stderr) = run(&["fmt", "-q", &a]);
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (0, "", ""));
}

/// A file already in canonical style is not rewritten: `fmt` used to run
/// its atomic replace on every file it read, so a clean tree came out
/// with a new inode, a new mtime and the fixer's ownership on every file
/// — every build tool downstream saw a change that was none. Pinned by
/// the inode and the mtime, on both reaches (the leaf, the door), against
/// the messy neighbour that IS replaced (a new inode: the write is
/// temp-and-rename, never in place).
#[test]
#[cfg(unix)]
fn fmt_leaves_a_canonical_file_untouched_on_disk() {
    use std::os::unix::fs::MetadataExt;
    let dir = scratch_dir("r106-fmt-clean");
    std::fs::write(dir.join("clean.nml"), "thing t:\n    a = 1\n").unwrap();
    std::fs::write(dir.join("messy.nml"), "thing u:\n    b =    2\n").unwrap();
    let stamp = |name: &str| {
        let m = std::fs::metadata(dir.join(name)).unwrap();
        (m.ino(), m.modified().unwrap(), m.mode())
    };
    let (clean_before, messy_before) = (stamp("clean.nml"), stamp("messy.nml"));
    // Past any filesystem's mtime granularity, so an in-place rewrite
    // could not hide behind an equal timestamp.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let clean = dir.join("clean.nml").display().to_string();
    let (code, stdout, _) = run(&["fmt", &clean]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.ends_with(": already in canonical style\n"),
        "{stdout}"
    );
    assert_eq!(
        stamp("clean.nml"),
        clean_before,
        "the leaf reach left the clean file alone"
    );
    let root = dir.to_str().unwrap().to_string();
    let (code, stdout, _) = run(&["fmt", "--root", &root, &root]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("formatted 1 of 2 file(s)\n"), "{stdout}");
    assert_eq!(
        stamp("clean.nml"),
        clean_before,
        "the door left the clean file alone"
    );
    let messy_after = stamp("messy.nml");
    assert_ne!(
        messy_after.0, messy_before.0,
        "the messy file was replaced, not rewritten"
    );
    assert_eq!(
        messy_after.2, messy_before.2,
        "its mode survives the replace"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("messy.nml")).unwrap(),
        "thing u:\n    b = 2\n"
    );
}

/// Under `--json` a `fmt` row per file (`changed` says whether the bytes
/// differ from canonical — on a dry run, whether a rewrite would change
/// them), `dryRun` on the closing row, and the gate's verdict as `exit`.
/// The wire's shape is the one revision 1 shipped: no new row, no new
/// field on the `fmt` row.
#[test]
fn fmt_json_is_one_row_per_file_with_dry_run_on_the_closing_row() {
    let dir = scratch_dir("r105-fmt-json");
    std::fs::write(dir.join("a.nml"), "thing t:\n    a =    1\n").unwrap();
    std::fs::write(dir.join("b.nml"), "thing u:\n    b = 2\n").unwrap();
    let root = dir.to_str().unwrap().to_string();
    let (code, rows) = json_rows(&["fmt", "--json", "--check", "--root", &root, &root]);
    assert_eq!(code, 1);
    let fmt: Vec<&serde_json::Value> = rows.iter().filter(|r| r["type"] == "fmt").collect();
    assert_eq!(fmt.len(), 2, "{rows:?}");
    for row in &fmt {
        assert_eq!(
            row.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["changed", "file", "type"],
            "the fmt row's shape: {row}"
        );
    }
    assert_eq!(fmt.iter().filter(|r| r["changed"] == true).count(), 1);
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["verb"], "fmt");
    assert_eq!(summary["dryRun"], true, "{summary}");
    assert_eq!(summary["exit"], 1);
    assert_eq!(summary["targets"], 2);
    let (_, rows) = json_rows(&["fmt", "--json", &dir.join("b.nml").display().to_string()]);
    assert_eq!(rows.last().unwrap()["dryRun"], false);
}

/// The two reaches. A bare file formats at its leaf with no universe:
/// a file in a directory the walk cannot list — an unlistable sibling
/// beside it — still formats, as every formatter in the state of the
/// art formats a named file. A directory target, or `--root`, opens the
/// door: the same tree is then the universe, and the unlistable
/// neighbour is its NML2089 — the walking verbs' verdict, `check`'s
/// exactly.
#[test]
#[cfg(unix)]
fn fmt_formats_a_bare_file_at_its_leaf_and_walks_a_tree_through_the_door() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("r105-fmt-reach");
    let messy = "thing t:\n    a =    1\n";
    std::fs::write(dir.join("a.nml"), messy).unwrap();
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("x.nml"), messy).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let a = dir.join("a.nml").display().to_string();
    let root = dir.to_str().unwrap().to_string();

    let (code, stdout, stderr) = run(&["fmt", &a]);
    let (check_code, _, check_err) = run(&["check", "--root", &root, &a]);
    let (door_code, _, door_err) = run(&["fmt", "--root", &root, &a]);
    let (tree_code, _, tree_err) = run(&["fmt", &root]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        code, 0,
        "the leaf reach needs no universe: {stdout}{stderr}"
    );
    assert!(stdout.contains("formatted "), "{stdout}");
    assert!(stderr.is_empty(), "no universe, no note: {stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nml")).unwrap(),
        "thing t:\n    a = 1\n"
    );
    // Through the door the tree is the universe — `check`'s verdict, exactly.
    assert_eq!(check_code, 1, "{check_err}");
    assert!(check_err.contains("error[NML2089]"), "{check_err}");
    assert_eq!(door_code, 1, "{door_err}");
    assert!(
        door_err.contains("error[NML2089]"),
        "--root opens the door: {door_err}"
    );
    assert_eq!(tree_code, 1, "{tree_err}");
    assert!(
        tree_err.contains("error[NML2089]"),
        "a directory opens the door: {tree_err}"
    );
}

/// Through the door the formatter keeps the universe's word: a closed
/// binding's rejected path is never opened (NML2083, as `check` says
/// it), and `--check` fails on content a directory walk skipped, as
/// `fix --check` does — a formatter that certified a tree it never
/// looked at would be a green gate over unformatted files.
#[test]
#[cfg(unix)]
fn fmt_through_the_door_keeps_the_universes_word() {
    let dir = scratch_dir("r105-fmt-closed");
    std::fs::write(
        dir.join("demo.package.nml"),
        "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
         file = \"core.model.nml\"\n\n[]validator validators:\n    - flows:\n        files:\n            - \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n",
    )
    .unwrap();
    std::fs::write(dir.join("core.model.nml"), "model thing:\n    a int\n").unwrap();
    std::fs::create_dir_all(dir.join("tenants/cu")).unwrap();
    std::fs::write(
        dir.join("tenants/cu/plain.flow.nml"),
        "thing t:\n    a =    1\n",
    )
    .unwrap();
    std::fs::write(dir.join("elsewhere.nml"), "thing e:\n    a = 1\n").unwrap();
    std::os::unix::fs::symlink("../../elsewhere.nml", dir.join("tenants/cu/link.flow.nml"))
        .unwrap();
    let root = dir.to_str().unwrap().to_string();
    let link = dir.join("tenants/cu/link.flow.nml").display().to_string();

    // The named link: the closed binding's rejection, never opened.
    let (code, _, stderr) = run(&["fmt", "--root", &root, &link]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2083]"), "{stderr}");
    // The walked tree: the link is content the walk skipped — the gate fails on it.
    let tenants = dir.join("tenants").display().to_string();
    let (code, stdout, stderr) = run(&["fmt", "--check", "--root", &root, &tenants]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("skipped path(s) hold content no verb judged"),
        "{stderr}"
    );
    assert!(
        stderr.contains("1 file(s) not in canonical style"),
        "{stderr}"
    );
    // The real run formats what the walk enumerated and leaves the link alone.
    let (code, stdout, _) = run(&["fmt", "--root", &root, &tenants]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(
        std::fs::read_to_string(dir.join("tenants/cu/plain.flow.nml")).unwrap(),
        "thing t:\n    a = 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("elsewhere.nml")).unwrap(),
        "thing e:\n    a = 1\n"
    );
}

/// r75 (r74-cert-cli F7, F8; the merge report's `binding`-row debt):
/// the closing row states the kernel's CLOSURE — `complete`,
/// `truncated`, `unloadable` — so `universe: "closed"` no longer
/// conflates the three; the `binding` row carries `closure` and
/// `truncatedUnits` too; and `binding`'s closing row counts the errors
/// its block reports (it said `errors: 0` while exiting 1 on a universe
/// error).
#[cfg(unix)]
#[test]
fn the_closing_row_states_the_closure_and_binding_counts_its_errors() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("r75-closure");
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    // complete
    let (code, r) = json_rows(&["check", "--json", "--root", &root, &plain]);
    assert_eq!(code, 0);
    assert_eq!(
        r.last().unwrap()["closure"],
        "complete",
        "{}",
        r.last().unwrap()
    );
    let (code, r) = json_rows(&["binding", "--json", "--root", &root, &plain]);
    assert_eq!(code, 0);
    let row = r.iter().find(|row| row["type"] == "binding").unwrap();
    assert_eq!(row["closure"], "complete", "{row}");
    assert_eq!(row["truncatedUnits"], serde_json::json!([]), "{row}");
    assert_eq!(r.last().unwrap()["errors"], 0);
    // unloadable: a manifest whose declared source is absent (NML2088).
    let broken = workspace_copy("r75-closure-unloadable");
    std::fs::remove_file(broken.join("core.model.nml")).unwrap();
    let broot = broken.to_str().unwrap().to_string();
    let bplain = broken
        .join("tenants/cu/plain.flow.nml")
        .display()
        .to_string();
    let (code, r) = json_rows(&["check", "--json", "--root", &broot, &bplain]);
    assert_eq!(code, 1);
    assert_eq!(
        r.last().unwrap()["closure"],
        "unloadable",
        "{}",
        r.last().unwrap()
    );
    assert_eq!(r.last().unwrap()["universe"], "closed");
    let (code, r) = json_rows(&["binding", "--json", "--root", &broot, &bplain]);
    assert_eq!(code, 1);
    let row = r.iter().find(|row| row["type"] == "binding").unwrap();
    assert_eq!(row["closure"], "unloadable", "{row}");
    assert!(
        r.last().unwrap()["errors"].as_u64().unwrap() >= 1,
        "binding counts the NML2088 rows it reports: {}",
        r.last().unwrap()
    );
    // truncated: a locked directory in the ROOT unit.
    let locked = dir.join("vendor/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _unlock = Unlock(locked.clone());
    if std::fs::read_dir(&locked).is_ok() {
        return; // root: the lock does not bite
    }
    let (code, r) = json_rows(&["check", "--json", "--root", &root, &plain]);
    assert_eq!(code, 1);
    assert_eq!(
        r.last().unwrap()["closure"],
        "truncated",
        "{}",
        r.last().unwrap()
    );
    assert_eq!(r.last().unwrap()["universe"], "closed");
    assert_eq!(r.last().unwrap()["truncatedUnits"], serde_json::json!([]));
    let (code, r) = json_rows(&["binding", "--json", "--root", &root, &plain]);
    assert_eq!(code, 1);
    let row = r.iter().find(|row| row["type"] == "binding").unwrap();
    assert_eq!(row["closure"], "truncated", "{row}");
    assert_eq!(
        r.last().unwrap()["errors"],
        1,
        "binding's universe error is counted: {}",
        r.last().unwrap()
    );
}

/// r75 (r74-kernel F7): a spent live-input budget names the INPUT that
/// crossed it, not a directory, and ends in the kernel's own remedy —
/// smaller live inputs — so the CLI appends no `--root` advice to it
/// (rooting elsewhere is not the remedy; the row keeps it for the
/// entry-bound and unreadable-directory shapes). Sixteen manifests in
/// content no glob reaches, each declaring a distinct 4 MiB SPARSE
/// source, spend the root unit's 64 MiB: the universe is closed-denied,
/// `closure: "truncated"`, no unit named.
#[cfg(unix)]
#[test]
fn a_spent_live_input_budget_names_the_input_and_offers_no_root() {
    let dir = workspace_copy("r75-bytes");
    for i in 0..17 {
        let vdir = dir.join(format!("vendor/v{i:02}"));
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(
            vdir.join(format!("m{i:02}.package.nml")),
            format!(
                "package m{i:02}:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
                 schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator \
                 validators:\n    - b:\n        files:\n            - \"never/*.q.nml\"\n        \
                 schemas:\n            - core\n        strict = true\n"
            ),
        )
        .unwrap();
        // Sparse: 4 MiB of zeros costs no disk and reads in milliseconds.
        std::fs::File::create(vdir.join("core.model.nml"))
            .unwrap()
            .set_len(4 * 1024 * 1024)
            .unwrap();
    }
    let root = dir.to_str().unwrap().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &plain]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error[NML2089]: cannot enumerate manifests: "),
        "{stderr}"
    );
    assert!(
        stderr.contains("the live-input budget (67108864 bytes) was spent reading `vendor/v"),
        "{stderr}"
    );
    assert!(
        stderr.contains("declared sources under the root"),
        "the kernel's own remedy: {stderr}"
    );
    assert!(!stderr.contains("--root"), "no --root advice: {stderr}");
    let (code, r) = json_rows(&["check", "--json", "--root", &root, &plain]);
    assert_eq!(code, 1);
    let last = r.last().unwrap();
    assert_eq!(last["closure"], "truncated", "{last}");
    assert_eq!(last["universe"], "closed", "{last}");
    assert_eq!(last["truncatedUnits"], serde_json::json!([]), "{last}");
    assert!(
        r.iter().any(|row| row["type"] == "diagnostic"
            && row["code"] == "NML2089"
            && row["message"]
                .as_str()
                .unwrap()
                .contains("live-input budget")),
        "{r:?}"
    );
}

// ---------------------------------------------------------------------
// r77 — the round-76 fold: resolve-once, the unit byte row, the byte
// backstop, the outside-root write, `explain --help`, the empty `fix`.
// ---------------------------------------------------------------------

/// A workspace whose operator's glob makes each tenant its own unit with
/// the tenant's `other/` live INSIDE it (`tenants/*/flows/*.flow.nml`),
/// `tenants` × `per_tenant` live manifests under `tenants/t<i>/other/v<j>/`
/// each declaring a distinct SPARSE 4 MiB source (zeros cost no disk and
/// read in milliseconds), and one flow file per tenant plus the
/// operator's `admin/ops.flow.nml`.
fn unit_bytes_workspace(tag: &str, tenants: &[&str], per_tenant: usize) -> Scratch {
    let dir = scratch_dir(tag);
    let manifest = |name: &str, globs: &[(&str, &str)]| {
        let mut text = format!(
            "package {name}:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
             schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator \
             validators:\n"
        );
        for (binding, glob) in globs {
            text.push_str(&format!(
                "    - {binding}:\n        files:\n            - \"{glob}\"\n        schemas:\n            \
                 - core\n        strict = true\n"
            ));
        }
        text
    };
    std::fs::write(
        dir.join("demo.package.nml"),
        manifest(
            "demo",
            &[
                ("tenantFlows", "tenants/*/flows/*.flow.nml"),
                ("ops", "admin/ops.flow.nml"),
            ],
        ),
    )
    .unwrap();
    std::fs::write(dir.join("core.model.nml"), "model thing:\n    v string\n").unwrap();
    let flow = "thing a:\n    v = \"x\"\n";
    std::fs::create_dir_all(dir.join("admin")).unwrap();
    std::fs::write(dir.join("admin/ops.flow.nml"), flow).unwrap();
    for t in tenants {
        let flows = dir.join(format!("tenants/{t}/flows"));
        std::fs::create_dir_all(&flows).unwrap();
        std::fs::write(flows.join("a.flow.nml"), flow).unwrap();
        for j in 0..per_tenant {
            let vdir = dir.join(format!("tenants/{t}/other/v{j:02}"));
            std::fs::create_dir_all(&vdir).unwrap();
            std::fs::write(
                vdir.join(format!("m{j:02}.package.nml")),
                manifest(&format!("m{j:02}"), &[("b", "never/*.q.nml")]),
            )
            .unwrap();
            std::fs::File::create(vdir.join("core.model.nml"))
                .unwrap()
                .set_len(4 * 1024 * 1024)
                .unwrap();
        }
    }
    dir
}

/// r77 (r76 F3): the UNIT byte-budget shape on the CLI, in the default
/// lane — `why: "liveInputBytes"` was pinned nowhere (the labels swapped
/// survived the lane). Seventeen sparse 4 MiB sources under `tenants/cu/
/// other/` spend the tenant's own 64 MiB at the sixteenth (the manifests'
/// bytes tip it): NML2089 on the KEY with the unit sentence and no
/// `--root`, `check`/`validate` exit 1, the sibling `ok` in the same
/// universe, and every row — `summary` and `binding` — carrying the unit
/// with its `why`.
#[test]
fn a_spent_unit_byte_budget_is_a_live_input_bytes_row_in_every_verb() {
    let dir = unit_bytes_workspace("r77-unit-bytes", &["cu", "du"], 0);
    for j in 0..17 {
        let vdir = dir.join(format!("tenants/cu/other/v{j:02}"));
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(
            vdir.join(format!("m{j:02}.package.nml")),
            format!(
                "package m{j:02}:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
                 schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator \
                 validators:\n    - b:\n        files:\n            - \"never/*.q.nml\"\n        \
                 schemas:\n            - core\n        strict = true\n"
            ),
        )
        .unwrap();
        std::fs::File::create(vdir.join("core.model.nml"))
            .unwrap()
            .set_len(4 * 1024 * 1024)
            .unwrap();
    }
    let root = dir.to_str().unwrap().to_string();
    let cu = dir
        .join("tenants/cu/flows/a.flow.nml")
        .display()
        .to_string();
    let du = dir
        .join("tenants/du/flows/a.flow.nml")
        .display()
        .to_string();
    let stop = "tenants/cu/other/v15/core.model.nml";
    let (code, stdout, stderr) = run(&["check", "--root", &root, &cu]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "tenants/cu/flows/a.flow.nml: error[NML2089]: the discovery budget for `tenants/cu` \
             is exhausted: the walk stopped at `{stop}` (the 67108864-byte live-input budget \
             for this subtree was spent reading it)"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "use fewer or smaller live manifests, project configs and declared sources under \
             `tenants/cu`"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("--root"), "{stderr}");
    assert!(!stderr.contains("cannot enumerate manifests"), "{stderr}");
    let (code, _, stderr) = run(&["validate", "--root", &root, &cu]);
    assert_eq!(code, 1, "{stderr}");
    let unit = serde_json::json!([{"unit": "tenants/cu", "stop": stop, "why": "liveInputBytes"}]);
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &cu]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["closure"], "complete", "{last}");
    assert_eq!(last["universe"], "closed", "{last}");
    assert_eq!(last["manifests"], 1, "{last}");
    assert_eq!(last["truncatedUnits"], unit, "{last}");
    assert!(
        rows.iter().any(|r| r["type"] == "diagnostic"
            && r["code"] == "NML2089"
            && r["message"]
                .as_str()
                .unwrap()
                .contains("live-input budget for this subtree")),
        "{rows:?}"
    );
    // The sibling binds in the same universe; its rows carry the unit.
    let (code, stdout, stderr) = run(&["check", "--root", &root, &du]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &du]);
    assert_eq!(code, 0);
    assert_eq!(rows.last().unwrap()["truncatedUnits"], unit);
    let (code, rows) = json_rows(&["binding", "--json", "--root", &root, &du]);
    assert_eq!(code, 0);
    let binding = rows
        .iter()
        .find(|r| r["type"] == "binding")
        .expect("a binding row");
    assert_eq!(binding["truncatedUnits"], unit, "{binding}");
    assert_eq!(binding["closure"], "complete", "{binding}");
    assert_eq!(binding["governing"], "bound", "{binding}");
    // A denied path could not be fixed (exit 1); the gate's 1.
    let (code, stdout, stderr) = run(&["fix", "--dry-run", "--root", &root, &cu]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("live-input budget for this subtree was spent"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&["fix", "--check", "--root", &root, &cu]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error: 1 path(s) could not be fixed"),
        "a denied path could not be fixed, under the gate too: {stderr}"
    );
}

/// r77 (r76 F1 (b)): the universe-wide byte BACKSTOP on the CLI —
/// sixteen tenants each spending their own 64 MiB (sparse sources) cross
/// `MAX_TOTAL_LIVE_INPUT_BYTES` at the last tenant's sixteenth source:
/// the whole universe, its own NML2089 sentence naming the total and the
/// input, the kernel's remedy, and — unlike the root unit's byte
/// sentence — the CLI's `--root` advice (a smaller tree IS a remedy for
/// sixteen units' worth of live inputs); `closure: "truncated"`,
/// `truncatedUnits: []`, every verb exit 1.
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_` \
            (256 sparse 4 MiB sources: 1 GiB read and held per run, r77)"]
fn perf_universe_byte_backstop_denies_everyone_and_offers_a_smaller_tree() {
    let tenants: Vec<String> = (0..16).map(|t| format!("t{t:02}")).collect();
    let names: Vec<&str> = tenants.iter().map(String::as_str).collect();
    let dir = unit_bytes_workspace("r77-backstop", &names, 16);
    let root = dir.to_str().unwrap().to_string();
    let t00 = dir
        .join("tenants/t00/flows/a.flow.nml")
        .display()
        .to_string();
    let stop = "tenants/t15/other/v15/core.model.nml";
    let (code, stdout, stderr) = run(&["check", "--root", &root, &t00]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("error[NML2089]: cannot enumerate manifests: "),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "the universe-wide live-input budget (1073741824 bytes, every budget unit summed) \
             was spent reading `{stop}`"
        )),
        "{stderr}"
    );
    // The kernel's byte-backstop row ends in its own remedy; the CLI
    // appends only its flag (r84-ux F7: `remove what stopped the walk`
    // is the kernel's clause on the entry-bound and unreadable rows).
    assert!(
        stderr.contains(
            "declared sources across the tree, or pass --root to a smaller tree that still \
             holds your manifests"
        ),
        "the kernel's remedy, then the CLI's flag: {stderr}"
    );
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &t00]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["closure"], "truncated", "{last}");
    assert_eq!(last["universe"], "closed", "{last}");
    assert_eq!(last["manifests"], 0, "{last}");
    assert_eq!(last["truncatedUnits"], serde_json::json!([]), "{last}");
    let ops = dir.join("admin/ops.flow.nml").display().to_string();
    let (code, _, stderr) = run(&["binding", "--root", &root, &ops]);
    assert_eq!(code, 1, "everyone is denied: {stderr}");
    let (code, _, stderr) = run(&["fix", "--dry-run", "--root", &root, &ops]);
    assert_eq!(code, 1, "a run-level universe error: {stderr}");
}

/// r77 (r76 F2): the leaf is resolved ONCE per invocation. `l.nml ->
/// a.nml`; `fmt l.nml` reads `a.nml`; while it formats, the link is
/// re-pointed at `b.nml`; the rewrite lands on `a.nml` — the file that
/// was READ — and `b.nml` is untouched. Pre-r77 the write resolved the
/// link a second time: `a.nml` unchanged, `a.nml`'s formatted text IN
/// `b.nml` (reproduced deterministically by the r76 certifier). A
/// 30,000-block file formats in ~1.5 s on the debug binary; the re-point
/// at 250 ms lands between the read (the first milliseconds) and the
/// write. The same for an open universe's `fix` through its own link.
/// The access time a file is stamped with before a child is spawned to
/// read it: `SystemTime::UNIX_EPOCH` plus a fixed offset, far older than
/// its write, so the mount's `relatime` rule (a read moves `atime` when
/// it is not later than `mtime`) records the child's read.
#[cfg(unix)]
fn stamp_unread(path: &Path) -> std::time::SystemTime {
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_accessed(old))
        .unwrap();
    old
}

/// Whether the mount records reads at all — a stamped file this process
/// reads itself must move. A `noatime` mount cannot host a pin that gates
/// on a child's read; the caller returns early rather than sleeping and
/// hoping (the pre-fold 250 ms sleep raced the child on a slow box and
/// passed vacuously on a fast one).
#[cfg(unix)]
fn mount_records_reads(dir: &Path) -> bool {
    let probe = dir.join(".atime-probe");
    std::fs::write(&probe, "x").unwrap();
    let unread = stamp_unread(&probe);
    let _ = std::fs::read(&probe).unwrap();
    let moved = std::fs::metadata(&probe).unwrap().accessed().unwrap() != unread;
    let _ = std::fs::remove_file(&probe);
    moved
}

/// Blocks until the child has READ `path` (its stamped access time
/// moved), bounded: a child that never reads is a failed pin, said so.
#[cfg(unix)]
fn wait_until_read(path: &Path, unread: std::time::SystemTime) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::fs::metadata(path).unwrap().accessed().unwrap() == unread {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never read {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
#[cfg(unix)]
fn fmt_and_fix_write_the_file_they_read_when_the_link_is_repointed_midway() {
    let dir = scratch_dir("r77-repoint");
    let messy: String = (0..30_000)
        .map(|i| format!("thing t{i}:\n    a =    {i}\n    b =     \"x\"\n"))
        .collect();
    std::fs::write(dir.join("a.nml"), &messy).unwrap();
    std::fs::write(dir.join("control.nml"), &messy).unwrap();
    let keep = "thing keep:\n    v =    9\n";
    std::fs::write(dir.join("b.nml"), keep).unwrap();
    std::os::unix::fs::symlink("a.nml", dir.join("l.nml")).unwrap();
    let (code, _, stderr) = run(&["fmt", dir.join("control.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    let expected = std::fs::read_to_string(dir.join("control.nml")).unwrap();
    assert_ne!(
        expected, messy,
        "the fixture must be something fmt rewrites"
    );
    let repoint = |link: &Path, target: &str| {
        // Atomic: a fresh link renamed over the old one.
        let swap = link.with_extension("swap");
        std::os::unix::fs::symlink(target, &swap).unwrap();
        std::fs::rename(&swap, link).unwrap();
    };
    if !mount_records_reads(&dir) {
        return; // a `noatime` mount: the read is unobservable here
    }
    let unread = stamp_unread(&dir.join("a.nml"));
    let child = nml_bin()
        .arg("fmt")
        .arg(dir.join("l.nml"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("fmt spawns");
    wait_until_read(&dir.join("a.nml"), unread);
    let repointed = std::time::SystemTime::now();
    repoint(&dir.join("l.nml"), "b.nml");
    let out = child.wait_with_output().expect("fmt ends");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::metadata(dir.join("a.nml"))
            .unwrap()
            .modified()
            .unwrap()
            >= repointed,
        "the rewrite landed AFTER the repoint — the window the pin claims was observed"
    );
    assert_eq!(
        std::fs::read_link(dir.join("l.nml")).unwrap(),
        Path::new("b.nml")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nml")).unwrap(),
        expected,
        "the file that was READ holds the rewrite"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.nml")).unwrap(),
        keep,
        "the link's NEW target is untouched"
    );

    // `fix` in an open universe: a large file with one fixable finding.
    let mut body = String::from("model m:\n    name string\n\n");
    for i in 0..30_000 {
        body.push_str(&format!("m X{i}:\n    name = \"x\"\n"));
    }
    body.push_str("m Y:\n    nme = \"y\"\n");
    std::fs::write(dir.join("fa.nml"), &body).unwrap();
    std::fs::write(dir.join("fcontrol.nml"), &body).unwrap();
    std::fs::write(dir.join("fb.nml"), keep).unwrap();
    std::os::unix::fs::symlink("fa.nml", dir.join("fl.nml")).unwrap();
    let (code, stdout, stderr) = run(&["fix", dir.join("fcontrol.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("1 edit(s) applied"), "{stdout}");
    let fixed = std::fs::read_to_string(dir.join("fcontrol.nml")).unwrap();
    assert_ne!(fixed, body);
    let unread = stamp_unread(&dir.join("fa.nml"));
    let child = nml_bin()
        .arg("fix")
        .arg(dir.join("fl.nml"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("fix spawns");
    wait_until_read(&dir.join("fa.nml"), unread);
    let repointed = std::time::SystemTime::now();
    repoint(&dir.join("fl.nml"), "fb.nml");
    let out = child.wait_with_output().expect("fix ends");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::metadata(dir.join("fa.nml"))
            .unwrap()
            .modified()
            .unwrap()
            >= repointed,
        "the fix landed AFTER the repoint — the window the pin claims was observed"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 edit(s) applied"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(std::fs::read_to_string(dir.join("fa.nml")).unwrap(), fixed);
    assert_eq!(std::fs::read_to_string(dir.join("fb.nml")).unwrap(), keep);
    assert_eq!(
        std::fs::read_link(dir.join("fl.nml")).unwrap(),
        Path::new("fb.nml")
    );
}

/// r77 (r76 F5, the r74 decision-1 sentence that never landed in E38 or
/// the guide): an open universe's write follows the operator's leaf link
/// wherever it points — INCLUDING outside the derived root. `ws/sub/
/// l.nml -> ../../outside/real.nml`; `fix --root ws` repairs
/// `outside/real.nml` in place with the link standing; `fmt` likewise.
#[test]
#[cfg(unix)]
fn an_open_universe_write_follows_the_leaf_link_outside_the_root() {
    let dir = scratch_dir("r77-outside");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let body = "model m:\n    name string\n\nm X:\n    nme = \"x\"\n";
    let messy = "thing t:\n    a =    1\n";
    std::fs::write(outside.join("real.nml"), body).unwrap();
    std::fs::write(outside.join("realf.nml"), messy).unwrap();
    std::os::unix::fs::symlink("../../outside/real.nml", ws.join("sub/l.nml")).unwrap();
    std::os::unix::fs::symlink("../../outside/realf.nml", ws.join("sub/lf.nml")).unwrap();
    // Controls: the same bytes, named directly.
    std::fs::write(dir.join("c.nml"), body).unwrap();
    std::fs::write(dir.join("cf.nml"), messy).unwrap();
    let (code, _, stderr) = run(&["fix", dir.join("c.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    let fixed = std::fs::read_to_string(dir.join("c.nml")).unwrap();
    assert_ne!(fixed, body);
    let (code, _, stderr) = run(&["fmt", dir.join("cf.nml").to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    let formatted = std::fs::read_to_string(dir.join("cf.nml")).unwrap();
    assert_ne!(formatted, messy);

    let root = ws.to_str().unwrap();
    let link = ws.join("sub/l.nml");
    let (code, stdout, stderr) = run(&["fix", "--root", root, link.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("1 edit(s) applied"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(outside.join("real.nml")).unwrap(),
        fixed,
        "the target OUTSIDE the root holds the fix"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("../../outside/real.nml")
    );
    let linkf = ws.join("sub/lf.nml");
    let (code, stdout, stderr) = run(&["fmt", linkf.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(
        std::fs::read_to_string(outside.join("realf.nml")).unwrap(),
        formatted
    );
    assert!(
        std::fs::symlink_metadata(&linkf)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// r77 (r76 F7): `explain`'s `--help` wins anywhere, before arity, as
/// every other verb's does — `nml explain --json --help` is a help
/// page, never a usage error; a bare `nml explain` is one (exit 2).
#[test]
fn explain_help_wins_before_arity() {
    for args in [
        vec!["explain", "--json", "--help"],
        vec!["explain", "NML2007", "-h"],
        vec!["explain", "--help"],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(
            stdout.starts_with("usage: nml explain [--json] [--quiet] <code>... | --list"),
            "{args:?}: {stdout}"
        );
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
    }
    let (code, _, stderr) = run(&["explain"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("usage: nml explain [--json] [--quiet] <code>... | --list"),
        "{stderr}"
    );
}

/// r77 (r76 F8): `fix` on a directory holding no `.nml` file still ends
/// with the closing row carrying `fix`'s own fields — zeros, never
/// absences (the guide: "`fix` carries its own fields … in the same
/// row", on every path).
#[test]
fn fix_on_a_directory_with_no_nml_file_still_carries_the_fix_fields() {
    let dir = scratch_dir("r77-emptydir");
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    let empty = dir.join("empty").display().to_string();
    // `dryRun` is pinned by VALUE on all three paths (r79, r78 F-c):
    // `--check` implies `--dry-run`; plain `fix` is not one.
    for (flags, dry_run) in [
        (&["--check"][..], true),
        (&["--dry-run"][..], true),
        (&[][..], false),
    ] {
        let mut args = vec!["fix"];
        args.extend_from_slice(flags);
        args.push(&empty);
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 1, "{flags:?}: {stderr}");
        assert!(
            stderr.contains("no .nml files found under `"),
            "{flags:?}: {stderr}"
        );
        let mut args = vec!["fix"];
        args.extend_from_slice(flags);
        args.extend(["--json", &empty]);
        let (code, rows) = json_rows(&args);
        assert_eq!(code, 1);
        let last = rows.last().unwrap();
        assert_eq!(last["type"], "summary", "{last}");
        assert_eq!(last["exit"], 1, "{last}");
        for field in [
            "edits",
            "filesFixed",
            "files",
            "remaining",
            "routed",
            "suppressed",
            "budgetExhausted",
            "failed",
        ] {
            assert_eq!(last[field], 0, "{flags:?}: {field}: {last}");
        }
        assert_eq!(last["dryRun"], dry_run, "{flags:?}: {last}");
    }
}

/// r87 (r86 F9): under a DERIVED root the outside-root sentence cannot
/// advise `--root` into a universe holding both files — it says the
/// other file is checked in its own run; under `--root` it does not.
#[test]
fn a_target_outside_a_derived_root_is_told_to_check_it_in_its_own_run() {
    let dir = scratch_dir("r87-two-roots");
    for repo in ["a", "b"] {
        std::fs::create_dir_all(dir.join(repo).join(".git")).unwrap();
        std::fs::write(dir.join(repo).join("x.nml"), "thing t:\n    a = 1\n").unwrap();
    }
    let a = dir.join("a/x.nml").display().to_string();
    let b = dir.join("b/x.nml").display().to_string();
    let (code, _, stderr) = run(&["check", &a, &b]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("is outside the workspace root")
            && stderr.contains("checked in its own run"),
        "{stderr}"
    );
    let root = dir.join("a").display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &b]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("is outside the workspace root")
            && !stderr.contains("checked in its own run"),
        "{stderr}"
    );
}

/// r89 (P11): the unit-layout lint is a universe note — ONCE per run
/// however many targets, attributed to the manifest, the same sentence
/// in every verb (`binding` lists it under `notes`), on the wire as a
/// locationless warning with the code — and an explicit `budgetUnits`
/// silences it.
#[test]
fn the_gap_lint_prints_once_per_run_and_a_declaration_silences_it() {
    let root = "tests/fixtures/workspace-gap";
    let file = "tests/fixtures/workspace-gap/tenants/cu/flows/plain.flow.nml";
    let (code, stdout, stderr) = run(&["check", "--root", root, file, file]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.matches(": ok (").count(), 1, "{stdout}");
    assert_eq!(
        stderr.matches("warning[NML2092]").count(),
        1,
        "once per run: {stderr}"
    );
    // Located in the manifest's own text — at the glob — as every
    // located finding prints (`key:line:col`), never a byte-span suffix.
    assert!(
        stderr.contains(
            "demo.package.nml:12:15: warning[NML2092]: binding 'tenantFlows' files[0] = \
             \"tenants/*/flows/**/*.flow.nml\": the inferred budget unit is `tenants/*/flows/*` \
             — content under `tenants/*` outside it stays in the root unit, where one tenant's \
             flood denies everyone; declare budgetUnits = [\"tenants/*\"] to isolate each \
             delegated subtree, or [\"tenants/*/flows/*\"] to keep the inferred boundary\n"
        ),
        "{stderr}"
    );
    let (code, rows) = json_rows(&["check", "--json", "--root", root, file]);
    assert_eq!(code, 0);
    let lint: Vec<&serde_json::Value> = rows.iter().filter(|r| r["code"] == "NML2092").collect();
    assert_eq!(lint.len(), 1, "{rows:?}");
    assert_eq!(lint[0]["source"], "demo.package.nml");
    assert_eq!(lint[0]["severity"], "warning");
    assert_eq!(
        (lint[0]["line"].as_u64(), lint[0]["col"].as_u64()),
        (Some(12), Some(15))
    );
    let summary = rows.last().unwrap();
    assert_eq!(summary["warnings"], 1, "{summary}");
    let (code, stdout, stderr) = run(&["binding", "--root", root, file]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("demo.package.nml:12:15: warning[NML2092]: binding 'tenantFlows'")
            && !stderr.contains(".."),
        "the universe's word, once, before the block: {stderr}"
    );
    assert!(
        !stdout.contains("NML2092"),
        "never inside a block: {stdout}"
    );
    let (_, rows) = json_rows(&["binding", "--json", "--root", root, file]);
    let note = &rows[0];
    assert_eq!(note["type"], "diagnostic", "before the binding row: {note}");
    assert_eq!(note["code"], "NML2092", "{note}");
    assert_eq!(
        (note["line"].as_u64(), note["col"].as_u64()),
        (Some(12), Some(15))
    );
    // `fix` opens its targets through the same door: the note once,
    // before any file — on a dry run too.
    let (code, _, stderr) = run(&["fix", "--dry-run", "--root", root, file]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr
            .matches("demo.package.nml:12:15: warning[NML2092]")
            .count(),
        1,
        "{stderr}"
    );
    // Declared: silent, and the file still binds.
    let root = "tests/fixtures/workspace-gap-declared";
    let file = "tests/fixtures/workspace-gap-declared/tenants/cu/flows/plain.flow.nml";
    let (code, stdout, stderr) = run(&["check", "--root", root, file]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    assert!(!stderr.contains("NML2092"), "{stderr}");
}

/// The universe's word — its errors (NML2088/NML2089) or, when it
/// stands, its layout notes (NML2092) — is stated ONCE per run by
/// `binding` exactly as by every verb: on stderr before the first block,
/// tallied once, as `diagnostic` rows before the first `binding` row
/// under `--json`, and the run's explain hint names it; a block's
/// `notes` carry what bears on THAT key only (an inert input on its
/// chain, the kernel's own finding). It used to print NML2092 in every
/// target's block and tally it per block (`warnings: 2` for two
/// targets), from a flat list the kernel no longer hands out.
#[test]
fn binding_states_the_universe_once_per_run_and_its_blocks_carry_key_level_rows_only() {
    let root = "tests/fixtures/workspace-gap";
    let file = "tests/fixtures/workspace-gap/tenants/cu/flows/plain.flow.nml";
    let (code, stdout, stderr) = run(&["binding", "--root", root, file, file]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("warning[NML2092]").count(),
        1,
        "once per run: {stderr}"
    );
    assert!(
        stderr.starts_with("demo.package.nml:12:15: warning[NML2092]"),
        "before the first block: {stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML2092"),
        "{stderr}"
    );
    assert_eq!(
        stdout
            .matches("file      tenants/cu/flows/plain.flow.nml")
            .count(),
        2,
        "{stdout}"
    );
    assert!(
        !stdout.contains("NML2092") && !stdout.contains("notes"),
        "a block carries key-level rows only: {stdout}"
    );
    let (code, rows) = json_rows(&["binding", "--json", "--root", root, file, file]);
    assert_eq!(code, 0);
    assert_eq!(rows[0]["type"], "diagnostic", "{}", rows[0]);
    assert_eq!(rows[0]["code"], "NML2092", "{}", rows[0]);
    assert_eq!(
        rows.iter().filter(|r| r["code"] == "NML2092").count(),
        1,
        "{rows:#?}"
    );
    let bindings: Vec<&serde_json::Value> =
        rows.iter().filter(|r| r["type"] == "binding").collect();
    assert_eq!(bindings.len(), 2, "{rows:#?}");
    assert!(
        bindings.iter().all(|b| b["notes"] == serde_json::json!([])),
        "{bindings:#?}"
    );
    let last = rows.last().unwrap();
    assert_eq!(last["warnings"], 1, "tallied once: {last}");
    // A key-level row stays in its block: the inert config on the chain.
    let dir = workspace_copy("binding-key-level");
    let (code, stdout, stderr) = run(&[
        "binding",
        "--root",
        dir.to_str().unwrap(),
        dir.join("tenants/cu/plain.flow.nml").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("notes     tenants/cu/nml-project.nml: warning[NML2080]"),
        "{stdout}"
    );
    assert!(
        !stderr.contains("warning[NML2080]"),
        "a key-level row is never on stderr: {stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML2080"),
        "the block's notes feed the run's explain hint: {stderr}"
    );
    // `-q`: the universe's warning is silent; the block stands.
    let (code, stdout, stderr) = run(&["binding", "-q", "--root", root, file]);
    assert_eq!(code, 0);
    assert_eq!(stderr, "", "{stderr}");
    assert!(stdout.contains("binding   tenantFlows"), "{stdout}");
}

/// r88 (P3): the invocation is wrong before the content is. A target
/// outside the root is refused BEFORE the universe walk runs, in every
/// workspace verb: no universe error prints, no `binding` block, and
/// the closing row carries the root (fixed first) and no universe
/// (never built). It used to walk, print the universe's own NML2088
/// and exit 1 (`check`/`validate`/`fix`), or print the first target's
/// whole block before refusing (`binding`).
#[test]
fn an_outside_target_is_refused_before_the_universe_walk() {
    let dir = scratch_dir("r88-outside-before-walk");
    for repo in ["a", "b"] {
        std::fs::create_dir_all(dir.join(repo).join(".git")).unwrap();
        std::fs::write(dir.join(repo).join("x.nml"), "thing t:\n    a = 1\n").unwrap();
    }
    // The universe under `a` cannot load: a walk that ran would say so.
    std::fs::write(dir.join("a/x.package.nml"), "not a manifest\n").unwrap();
    let a = dir.join("a/x.nml").display().to_string();
    let b = dir.join("b/x.nml").display().to_string();
    for verb in [
        vec!["check"],
        vec!["validate"],
        vec!["fix", "--check"],
        vec!["binding"],
    ] {
        let mut args = verb.clone();
        args.push(&a);
        args.push(&b);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 2, "{verb:?}: {stderr}");
        assert!(
            stderr.contains("is outside the workspace root"),
            "{verb:?}: {stderr}"
        );
        assert!(
            !stderr.contains("NML2088") && !stdout.contains("NML2088"),
            "{verb:?}: the walk ran: {stdout}{stderr}"
        );
        assert!(stdout.is_empty(), "{verb:?}: {stdout}");
    }
    let (code, r) = json_rows(&["check", "--json", &a, &b]);
    assert_eq!(code, 2);
    let last = r.last().unwrap();
    assert_eq!(last["type"], "summary");
    assert!(
        last["root"]["path"].as_str().unwrap().ends_with("/a"),
        "{last}"
    );
    assert!(
        last["universe"].is_null() && last["closure"].is_null() && last["manifests"].is_null(),
        "the walk ran: {last}"
    );
}

/// r88 (P3): the pre-walk classification is the kernel's own
/// (`SourceKey::classify`, closed trust), never a lexical rule — a link
/// followed by enough `..` to leave the root LEXICALLY is the closed
/// walk's halt at the link (NML2083, exit 1), not an outside-root
/// refusal: the walk's verdict is the one the front end gives.
#[cfg(unix)]
#[test]
fn a_link_then_dotdot_is_the_walks_halt_not_a_lexical_escape() {
    let dir = workspace_copy("r88-link-then-dotdot");
    std::os::unix::fs::symlink("../../vendor", dir.join("tenants/cu/lib")).unwrap();
    let root = dir.display().to_string();
    let spelled = dir
        .join("tenants/cu/lib/../../../../x.nml")
        .display()
        .to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &spelled]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("NML2083") && stderr.contains("`lib` is a symlink"),
        "{stderr}"
    );
    assert!(!stderr.contains("outside the workspace root"), "{stderr}");
}

/// r88 (P5, reconciled): the CLI's OWN lines spell the root relative to
/// the working directory when one contains the other — `.` at the root,
/// `..` from inside it, `<name>` from above it (git's
/// `status.relativePaths`): `binding`'s `root` line and the outside-root
/// refusal here — while a KERNEL sentence (NML2064's closed form) keeps
/// the canonical path it prints in the editor too, and every wire row
/// keeps the canonical path wherever the run stands.
#[test]
fn the_clis_own_lines_spell_the_root_from_the_working_directory() {
    let dir = workspace_copy("r88-relative-root");
    let root = std::fs::canonicalize(&*dir).unwrap();
    let canonical = root.display().to_string();
    let name = root.file_name().unwrap().to_str().unwrap().to_string();
    let file = "tenants/cu/plain.flow.nml";
    let (code, out, err) = run_in(&root, &["binding", "--root", ".", file]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\nroot      .  (--root)\n"), "{out}");
    let (_, out, _) = run_in(
        &root.join("tenants"),
        &["binding", "--root", "..", "cu/plain.flow.nml"],
    );
    assert!(out.contains("\nroot      ..  (--root)\n"), "{out}");
    let (_, out, _) = run_in(
        root.parent().unwrap(),
        &["binding", "--root", &name, &format!("{name}/{file}")],
    );
    assert!(
        out.contains(&format!("\nroot      {name}  (--root)\n")),
        "{out}"
    );
    let (_, _, err) = run_in(&root, &["check", "--root", ".", "../outside.nml"]);
    assert!(
        err.contains("is outside the workspace root `.` (--root)"),
        "{err}"
    );
    // A kernel sentence is one text for both front ends: it names no
    // root at all (r89 D8) — the root is the run's fact, above.
    let (_, _, err) = run_in(&root, &["check", "--root", ".", "docs/unclaimed.nml"]);
    assert!(
        err.contains("in the closed universe (2 manifest(s) discovered)")
            && !err.contains(&canonical),
        "{err}"
    );
    // The wire: canonical from every vantage point.
    for (cwd, spelled) in [(root.clone(), "."), (root.join("tenants"), "..")] {
        let (_, out, _) = run_in(
            &cwd,
            &["binding", "--json", "--root", spelled, "docs/unclaimed.nml"],
        );
        for row in out
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        {
            if let Some(path) = row["root"]["path"].as_str() {
                assert_eq!(path, canonical, "{row}");
            }
        }
    }
}

/// r88 (P6): the tool's typography renders as ASCII when the locale's
/// codeset is not UTF-8 (cargo's `term.unicode` model, POSIX precedence
/// `LC_ALL` > `LC_CTYPE` > `LANG`), `NML_UNICODE=0|1` overrides either
/// way, content past the fold table is spelled `\u{XXXX}` rather than
/// respelled, a `fix --dry-run` diff is the file's own bytes whatever
/// the locale, and the JSON stream is UTF-8 whatever the locale.
#[test]
fn the_tools_typography_folds_to_ascii_outside_a_utf8_locale() {
    let dir = workspace_copy("r88-ascii");
    let root = dir.display().to_string();
    let target = dir.join("docs/unclaimed.nml").display().to_string();
    let c: &[(&str, Option<&str>)] = &[("LC_ALL", Some("C")), ("LC_CTYPE", None), ("LANG", None)];
    let utf8: &[(&str, Option<&str>)] = &[("LC_ALL", Some("C.UTF-8"))];
    // NML2064's sentence carries an em dash.
    let (_, _, err) = run_env(utf8, &["check", "--root", &root, &target]);
    assert!(err.contains(" — add a `files` glob"), "{err}");
    let (_, _, err) = run_env(c, &["check", "--root", &root, &target]);
    if cfg!(windows) {
        assert!(
            err.contains(" — add a `files` glob"),
            "windows is unicode: {err}"
        );
    } else {
        assert!(
            err.contains(" -- add a `files` glob") && !err.contains('—'),
            "{err}"
        );
    }
    let (_, _, err) = run_env(
        &[("LC_ALL", Some("C")), ("NML_UNICODE", Some("1"))],
        &["check", "--root", &root, &target],
    );
    assert!(err.contains(" — add a `files` glob"), "{err}");
    let (_, _, err) = run_env(
        &[("LC_ALL", Some("C.UTF-8")), ("NML_UNICODE", Some("0"))],
        &["check", "--root", &root, &target],
    );
    assert!(
        err.contains(" -- add a `files` glob") && err.is_ascii(),
        "{err}"
    );
    // Content is escaped, never respelled: a walked name past the table.
    std::fs::write(dir.join("caf\u{e9}.nml"), "thing t\n    a = = 1\n").unwrap();
    let (_, _, err) = run_env(
        &[("NML_UNICODE", Some("0"))],
        &[
            "check",
            "--root",
            &root,
            &dir.join("caf\u{e9}.nml").display().to_string(),
        ],
    );
    assert!(err.contains("caf\\u{e9}.nml:") && err.is_ascii(), "{err}");
    // The wire keeps UTF-8.
    let (_, out, _) = run_env(
        &[("NML_UNICODE", Some("0"))],
        &["check", "--json", "--root", &root, &target],
    );
    assert!(out.contains(" — add a `files` glob"), "{out}");
    // A diff is the file's own bytes: the arm's `=>` is fixed to `->`,
    // and the changed line carries its em dash verbatim under ASCII
    // mode, while the same run's NML2080 warning (prose) folds.
    std::fs::write(
        dir.join("tenants/cu/dash.flow.nml"),
        "oneof email by kind:\n    \"a — b\" => emailLog\n\nmodel emailLog:\n    path string?\n",
    )
    .unwrap();
    let (_, out, err) = run_env(
        &[("NML_UNICODE", Some("0"))],
        &[
            "fix",
            "--dry-run",
            "--root",
            &root,
            &dir.join("tenants/cu/dash.flow.nml").display().to_string(),
        ],
    );
    assert!(
        out.contains("-    \"a — b\" => emailLog\n+    \"a — b\" -> emailLog\n"),
        "{out}"
    );
    assert!(
        err.contains(" -- content, not configuration") && err.is_ascii(),
        "{err}"
    );
}

/// r79 (r78 F-a): a trailing separator on a regular file's name is
/// insignificant on every verb — `f.nml/`, `f.nml/.` and `f.nml//` name
/// `f.nml`, the same key, opened beneath the same resolved leaf (r75's
/// open universe `check`/`fix` opened by path and refused them ENOTDIR;
/// `parse`/`fmt` and the closed universe already accepted them). A
/// directory typed with a trailing slash is still refused before any
/// read by the verbs that take files.
#[test]
fn a_trailing_separator_on_a_regular_file_names_the_same_key_in_every_verb() {
    let dir = scratch_dir("r79-trailing-separator");
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let canonical = "thing t:\n    a = 1\n";
    std::fs::write(ws.join("f.nml"), canonical).unwrap();
    let root = ws.display().to_string();
    let plain = ws.join("f.nml").display().to_string();
    // (the verb and its options, the exit on `f.nml`): `binding` in an
    // open universe reports the key unbound (exit 1) without opening
    // the file; `parse` and `fmt` are workspace-free.
    let lanes: [(&[&str], i32); 6] = [
        (&["check", "--root", &root], 0),
        (&["validate", "--root", &root], 0),
        (&["fix", "--check", "--root", &root], 0),
        (&["binding", "--root", &root], 1),
        (&["parse"], 0),
        (&["fmt"], 0),
    ];
    for (lane, exit) in lanes {
        let (code, plain_out, err) = run(&[lane, &[&plain]].concat());
        assert_eq!(code, exit, "{lane:?} f.nml: {err}");
        for spelling in ["f.nml/", "f.nml/.", "f.nml//"] {
            let target = format!("{root}/{spelling}");
            let (code, out, err) = run(&[lane, &[&target]].concat());
            assert_eq!(code, exit, "{lane:?} {spelling}: {err}");
            assert_eq!(
                out.replace(&target, &plain),
                plain_out,
                "{lane:?} {spelling}: the same key, the same report"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(ws.join("f.nml")).unwrap(),
        canonical
    );
    // A directory typed with a trailing slash: the workspace-free verb
    // refuses it before any read; the walking verbs (`fmt` on a
    // directory among them) walk it and, finding no `.nml` file in the
    // empty `sub/`, say so.
    let sub = format!("{root}/sub/");
    let file_verbs: [&[&str]; 1] = [&["parse"]];
    for lane in file_verbs {
        let (code, _, err) = run(&[lane, &[&sub]].concat());
        assert_eq!(code, 1, "{lane:?} sub/: {err}");
        assert!(
            err.to_lowercase().contains("is a directory"),
            "{lane:?} sub/: {err}"
        );
    }
    let walking: [&[&str]; 3] = [
        &["check", "--root", &root],
        &["validate", "--root", &root],
        &["fmt", "--root", &root],
    ];
    for lane in walking {
        let (code, _, err) = run(&[lane, &[&sub]].concat());
        assert_eq!(code, 1, "{lane:?} sub/: {err}");
        assert!(
            err.contains("error: no .nml files found under `"),
            "{lane:?} sub/: {err}"
        );
    }
}

// ---------------------------------------------------------------------
// The operator surface: `nml help <verb>`, both flag spellings and `--`,
// lenient `explain` spellings and its `--json` row, and the
// self-describing NDJSON stream (`formatVersion`, `nmlVersion`, the
// `error` row's `kind`).
// ---------------------------------------------------------------------

/// `nml help <verb>` IS `nml <verb> --help` (clig.dev: the two spellings
/// of one page), never the top-level page. Every page documents its exit
/// codes and leads with examples.
#[test]
fn help_verb_is_the_verbs_help_page() {
    for verb in [
        "check", "validate", "fix", "binding", "explain", "limits", "parse", "fmt",
    ] {
        let (code, stdout, stderr) = run(&["help", verb]);
        assert_eq!(code, 0, "{verb}: {stderr}");
        let (_, direct, _) = run(&[verb, "--help"]);
        assert_eq!(
            stdout, direct,
            "{verb}: help <verb> and <verb> --help differ"
        );
        assert!(
            stdout.contains("EXIT CODES:") && stdout.contains("EXAMPLES:"),
            "{verb}: {stdout}"
        );
    }
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("EXIT CODES:") && stdout.contains("EXAMPLES:"),
        "{stdout}"
    );
    let (code, _, stderr) = run(&["help", "bogus"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: unknown command: bogus"),
        "{stderr}"
    );
    // No arguments at all is the same usage error: the page on stderr.
    let (code, stdout, stderr) = run(&[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty() && stderr.contains("USAGE:"), "{stderr}");
    // `help` asked about itself is the top-level page, exit 0 — never a
    // recursion (r84: `nml help help` overflowed the stack, exit 134).
    for argv in [
        &["help", "help"][..],
        &["help", "--help"],
        &["help", "-h"],
        &["--help", "--help"],
        &["help", "help", "check"],
    ] {
        let (code, stdout, stderr) = run(argv);
        assert_eq!(code, 0, "{argv:?}: {stderr}");
        assert!(
            stdout.contains("USAGE:") && stderr.is_empty(),
            "{argv:?}: {stdout}{stderr}"
        );
    }
}

/// `nml help help`, `nml help --help`, `nml --help --help` and `nml -h -h`
/// are the top-level page, exit 0 — each forwarded to `nml --help --help`
/// and recursed until the stack overflowed (exit 134) — and a flag where
/// the command goes is a usage error that says where flags go, never
/// "unknown command: --json".
#[test]
fn help_help_and_a_top_level_flag_never_recurse() {
    for args in [
        vec!["help", "help"],
        vec!["help", "--help"],
        vec!["--help", "--help"],
        vec!["-h", "-h"],
        vec!["help", "--json"],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(
            stdout.contains("USAGE:") && stdout.contains("EXAMPLES:"),
            "{args:?}: {stdout}"
        );
    }
    // `help version` is the verb's own page (r85 P1: `version` has a
    // `Spec` like every verb), never the top-level page.
    let (code, stdout, stderr) = run(&["help", "version"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("usage: nml version [--json] [--quiet]") && stdout.contains("EXAMPLES:"),
        "{stdout}"
    );
    for args in [vec!["--json"], vec!["--json", "check", "x.nml"], vec!["-x"]] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 2, "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(
            stderr.starts_with("unknown flag ") && stderr.contains("the flags follow the command"),
            "{args:?}: {stderr}"
        );
    }
}

/// `--flag=value` and `--flag value` are one flag, `--` ends the flags,
/// and a value on a boolean flag is refused by name (`--root=.` is never
/// "unknown flag --root=.").
#[test]
fn both_flag_spellings_and_double_dash_end_the_flags() {
    let dir = workspace_copy("flag-spellings");
    let root = dir.to_str().unwrap();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&[
        "check",
        &format!("--root={root}"),
        "--max-findings=3",
        "--",
        &plain,
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    let (code, _, stderr) = run(&["check", "--strict=yes", &plain]);
    assert_eq!(code, 2, "a usage error: {stderr}");
    assert!(
        stderr.contains("error: --strict takes no value"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&["parse", "--", "--json"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("failed to read --json"),
        "after `--` the flag spelling is a file name: {stderr}"
    );
}

/// `2087` and `nml2087` name NML2087 (the digits are what the reader
/// copied from a log), and `--json` emits one `explain` row per code —
/// `{code, summary, document}` — then the closing row.
#[test]
fn explain_accepts_bare_digits_and_emits_json_rows() {
    for spelling in ["2087", "nml2087", "NML2087"] {
        let (code, stdout, stderr) = run(&["explain", spelling]);
        assert_eq!(code, 0, "{spelling}: {stderr}");
        assert!(stdout.starts_with("# NML2087"), "{spelling}: {stdout}");
    }
    let (code, rows) = json_rows(&["explain", "--json", "2087"]);
    assert_eq!(code, 0);
    assert_eq!(rows[0]["type"], "explain");
    assert_eq!(rows[0]["code"], "NML2087");
    assert!(
        rows[0]["document"]
            .as_str()
            .unwrap()
            .starts_with("# NML2087"),
        "{}",
        rows[0]
    );
    assert!(
        rows[0]["summary"]
            .as_str()
            .unwrap()
            .contains("Ambiguously claimed file"),
        "{}",
        rows[0]
    );
    assert_eq!(rows.last().unwrap()["type"], "summary");
    let (code, rows) = json_rows(&["explain", "--json", "--list"]);
    assert_eq!(code, 0);
    assert!(
        rows.iter().filter(|r| r["type"] == "explain").count() > 100,
        "{}",
        rows.len()
    );
    // The wire keeps the first-paragraph summary (r105-ux P3 changed the
    // human list only).
    assert!(
        rows.iter()
            .filter(|r| r["type"] == "explain")
            .all(|r| r["summary"].as_str().is_some_and(|s| s.starts_with("**"))),
        "the explain row's summary is the paragraph, bold lead included"
    );
    // The human list: one headline per code, every line a terminal line.
    let (code, stdout, _) = run(&["explain", "--list"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.len() > 100, "{}", lines.len());
    for line in &lines {
        assert!(
            line.len() > 9
                && line.starts_with("NML")
                && &line[7..9] == "  "
                && !line.contains("**"),
            "{line}"
        );
        assert!(
            line.chars().count() <= 90,
            "{} columns: {line}",
            line.chars().count()
        );
    }
    let (code, rows) = json_rows(&["explain", "--json", "NML9999"]);
    assert_eq!(code, 1);
    assert_eq!(rows[0]["type"], "error");
    assert_eq!(rows[0]["kind"], "run", "{}", rows[0]);
}

/// The stream says what wrote it and which contract it follows, and an
/// `error` row says what it is: `usage` (the invocation), `target` (one
/// target of a many-target run), `run` (the closing verdict).
#[test]
fn the_ndjson_stream_describes_itself() {
    let dir = workspace_copy("json-self-describing");
    let root = dir.to_str().unwrap();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let nope = dir.join("nope.nml").display().to_string();
    let (code, rows) = json_rows(&["check", "--json", "--root", root, &plain, &nope]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary");
    assert_eq!(last["formatVersion"], 1, "{last}");
    assert_eq!(last["nmlVersion"], env!("CARGO_PKG_VERSION"), "{last}");
    let target = rows
        .iter()
        .find(|r| r["type"] == "error")
        .expect("the absent target's row");
    assert_eq!(target["kind"], "target", "{target}");
    for args in [
        vec!["check", "--json", "--bogus", &plain],
        vec!["check", "--json"],
        vec!["binding", "--json"],
        vec!["parse", "--json"],
        vec!["limits", "--json", "--bogus"],
        vec!["explain", "--json"],
    ] {
        let (_, rows) = json_rows(&args);
        assert_eq!(rows[0]["type"], "error", "{args:?}: {}", rows[0]);
        assert_eq!(rows[0]["kind"], "usage", "{args:?}: {}", rows[0]);
        assert_eq!(rows.last().unwrap()["formatVersion"], 1, "{args:?}");
    }
    let shared = dir.join("shared/x.flow.nml").display().to_string();
    let (code, rows) = json_rows(&["check", "--json", "--root", root, &shared]);
    assert_eq!(code, 1);
    let verdict = rows
        .iter()
        .find(|r| r["type"] == "error")
        .expect("the closing verdict");
    assert_eq!(verdict["kind"], "run", "{verdict}");
    let (code, rows) = json_rows(&["check", "--json", "--root", root, "--schema", root, &plain]);
    assert_eq!(code, 2);
    let conflict = rows
        .iter()
        .find(|r| r["type"] == "error")
        .expect("the conflict");
    assert_eq!(conflict["kind"], "usage", "{conflict}");
}

// ---------------------------------------------------------------------
// The operator surface, continued: `-q/--quiet` on every verb, usage
// errors exiting 2 in every verb, `fmt`'s parse errors, `binding` on a
// directory, the expansion's argument order, the `--json` vocabulary.
// ---------------------------------------------------------------------

/// `-q`/`--quiet` prints errors only: no warning, no info, no explain
/// hint, no per-file `ok` line — in every verb, human and `--json`
/// alike — while the closing row's counts stay exact; after `--` the
/// spelling is a file name.
#[test]
fn quiet_prints_errors_only_and_keeps_the_counts_exact() {
    let dir = workspace_copy("quiet");
    let root = dir.to_str().unwrap();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let bad = dir.join("tenants/cu/bad.flow.nml").display().to_string();
    // A clean file under an inert config: three NML2080 warnings and the
    // hint print without the flag; nothing at all with it.
    let (code, stdout, stderr) = run(&["check", "--root", root, &plain]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("warning[NML2080]"), "{stderr}");
    assert!(stderr.contains("for more information"), "{stderr}");
    for quiet in ["-q", "--quiet"] {
        let (code, stdout_q, stderr_q) = run(&["check", quiet, "--root", root, &plain]);
        assert_eq!(code, 0, "{stderr_q}");
        assert_eq!(stderr_q, "", "{quiet}: errors only: {stderr_q}");
        assert_eq!(
            stdout_q, "",
            "{quiet}: the per-file ok line is success output, silenced"
        );
        assert!(stdout.contains(": ok ("), "{stdout}");
    }
    // Errors still print; the hint does not; the exit is the same.
    let (code, _, stderr) = run(&["check", "-q", "--root", root, &bad]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2008]"), "{stderr}");
    assert!(!stderr.contains("warning["), "{stderr}");
    assert!(!stderr.contains("for more information"), "{stderr}");
    // `--json`: no warning row, the exact count on the closing row.
    let (code, rows) = json_rows(&["check", "--json", "-q", "--root", root, &plain]);
    assert_eq!(code, 0);
    assert!(
        rows.iter().all(|r| r["type"] != "diagnostic"),
        "no warning row under --quiet: {rows:#?}"
    );
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary");
    assert!(
        last["warnings"].as_u64().unwrap() >= 1,
        "counted, not printed: {last}"
    );
    assert_eq!(
        last["withheld"],
        serde_json::Value::Null,
        "quiet is not the budget: {last}"
    );
    // `binding`: the block's notes keep only the errors.
    let (code, stdout, _) = run(&["binding", "--root", root, &plain]);
    assert_eq!(code, 0);
    assert!(stdout.contains("notes"), "{stdout}");
    let (code, stdout, _) = run(&["binding", "-q", "--root", root, &plain]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("notes"), "{stdout}");
    assert!(stdout.contains("binding   tenantFlows"), "{stdout}");
    // Every verb accepts it.
    for args in [
        vec!["validate", "-q", "--root", root, &plain],
        vec!["fix", "--dry-run", "-q", "--root", root, &plain],
        vec!["parse", "-q", &plain],
        vec!["explain", "-q", "NML2007"],
        vec!["limits", "-q"],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(!stderr.contains("warning["), "{args:?}: {stderr}");
    }
    let (code, _, stderr) = run(&["parse", "--", "-q"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("failed to read -q"),
        "after `--` it is a file name: {stderr}"
    );
}

/// A run the operator asked of a SET — a directory named, or more than
/// one path — ends with ONE closing line when every file passed,
/// `checked N file(s): N ok` (`validated …`), on stdout: the brief
/// success output clig.dev asks for (a 5,002-file run printed 5,002 `ok`
/// lines and no tally). A single file's own `ok` line is its whole
/// verdict (no tally); a failing set-run keeps `error: K of N file(s)
/// failed` as its one closing line; `--json` has the `summary` row
/// instead; `-q` silences the per-file `ok` lines, the tally and `fix`'s
/// per-file and closing lines — never a finding, never an exit code,
/// never a `--json` row.
#[test]
fn a_run_over_a_set_ends_with_one_closing_line_and_quiet_silences_success() {
    let root = "tests/fixtures/workspace-units";
    let tenants = "tests/fixtures/workspace-units/tenants";
    let cu = "tests/fixtures/workspace-units/tenants/cu/plain.flow.nml";
    let du = "tests/fixtures/workspace-units/tenants/du/plain.flow.nml";
    let (code, stdout, stderr) = run(&["check", "--root", root, tenants]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.ends_with("checked 2 file(s): 2 ok\n"), "{stdout}");
    assert_eq!(stdout.matches(": ok (").count(), 2, "{stdout}");
    let (code, stdout, _) = run(&["validate", "--root", root, cu, du]);
    assert_eq!(code, 0);
    assert!(stdout.ends_with("validated 2 file(s): 2 ok\n"), "{stdout}");
    // One file: its own line is the verdict.
    let (code, stdout, _) = run(&["check", "--root", root, cu]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("checked"), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    // A directory holding ONE file is a set by the invocation's SHAPE:
    // the tally prints however many files the directory expanded to.
    let one_dir = "tests/fixtures/workspace-units/tenants/cu";
    let (code, stdout, stderr) = run(&["check", "--root", root, one_dir]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.ends_with("checked 1 file(s): 1 ok\n"), "{stdout}");
    // A failing set-run: the failure line is the closing line, no tally.
    let dir = workspace_copy("set-tally-fail");
    let droot = dir.to_str().unwrap().to_string();
    let dtenants = dir.join("tenants").display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &droot, &dtenants]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(!stdout.contains("checked "), "{stdout}");
    assert!(stderr.trim_end().ends_with("file(s) failed"), "{stderr}");
    // `--json`: the summary row, never a human line.
    let (code, rows) = json_rows(&["check", "--json", "--root", root, tenants]);
    assert_eq!(code, 0);
    assert_eq!(
        rows.last().unwrap()["targets"],
        2,
        "{}",
        rows.last().unwrap()
    );
    // `-q`: nothing on success — check, validate and fix alike.
    for args in [
        vec!["check", "-q", "--root", root, tenants],
        vec!["validate", "-q", "--root", root, cu, du],
        vec!["fix", "--dry-run", "-q", "--root", root, tenants],
        vec!["fix", "--check", "-q", "--root", root, tenants],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert_eq!(stdout, "", "{args:?}: no output on success: {stdout}");
        assert_eq!(stderr, "", "{args:?}: {stderr}");
    }
    // `-q` silences `fix`'s per-file `would fix`/`fixed` line and its tally
    // on a REAL edit too: the file is rewritten and nothing is said.
    let scratch = scratch_dir("quiet-fix");
    let body = "model m:\n    name string\n\nm X:\n    nme = \"x\"\n";
    std::fs::write(scratch.join("fixable.nml"), body).unwrap();
    let fixable = scratch.join("fixable.nml").display().to_string();
    let (code, stdout, stderr) = run(&["fix", "--dry-run", "-q", &fixable]);
    assert_eq!(code, 0, "{stderr}");
    // The diff is the verb's ANSWER and stays; the `would fix` line and
    // the tally are success output and go.
    assert!(stdout.starts_with("--- a/"), "the diff stays: {stdout}");
    assert!(
        !stdout.contains("would fix"),
        "the would-fix line is silenced: {stdout}"
    );
    assert!(
        !stdout.contains("edit(s)"),
        "the tally is silenced: {stdout}"
    );
    assert_eq!(stderr, "", "{stderr}");
    let (code, stdout, stderr) = run(&["fix", "-q", &fixable]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains("fixed "),
        "the fixed line is silenced: {stdout}"
    );
    assert!(
        !stdout.contains("edit(s)"),
        "the tally is silenced: {stdout}"
    );
    assert_eq!(stderr, "", "{stderr}");
    assert_ne!(
        std::fs::read_to_string(scratch.join("fixable.nml")).unwrap(),
        body,
        "the file was rewritten"
    );
    // `-q` never silences a finding, the failure line or the exit.
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let bad = dir.join("tenants/cu/bad.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", "-q", "--root", &droot, &plain, &bad]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(stdout, "", "{stdout}");
    assert!(stderr.contains("error[NML2008]"), "{stderr}");
    assert!(stderr.contains("error: 1 of 2 file(s) failed"), "{stderr}");
}

/// A finding's remedy block — rustc's `help:` with a suggested
/// replacement, the kernel's structured insertion — prints beneath the
/// row and its notes RESOLVED against the manifest: `help:` names the
/// file and the line the block goes after, then the block at the
/// binding body's own indentation, on stderr; and the `--json` row
/// carries the same resolved edit in `suggestions[]` (the manifest's
/// key, the insertion point, the lines) while its `message` stays the
/// one line it was. NML2064's no-grant form is the first producer — the
/// `layers:` block, its `allowRefs` entry the key the clause reaches
/// (the file's own while its refs are same-file — the kernel's rule).
#[test]
fn the_remedy_block_prints_beneath_the_row_resolved_and_rides_the_wire() {
    the_help_snippet_pasted_as_printed_is_the_grant();
    let (code, _, stderr) = check_in_workspace("tenants/cu/member-lookup.flow.nml");
    assert_eq!(code, 1, "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let row = lines
        .iter()
        .position(|l| l.contains("error[NML2064]"))
        .unwrap_or_else(|| panic!("the row: {stderr}"));
    assert_eq!(
        lines[row + 1],
        "demo.package.nml:10:7: note: to permit it, give this binding a `layers:` grant whose \
         `allowRefs` admits \"tenants/cu/member-lookup.flow.nml\"",
        "the located remedy note sits between the row and its block: {stderr}"
    );
    assert_eq!(
        lines[row + 2],
        "help: the block to add after line 15 of demo.package.nml — paste it as printed:",
        "{stderr}"
    );
    // The block at the binding body's own indentation, read from the
    // manifest — a terminal copy pastes as printed.
    assert_eq!(lines[row + 3], "        layers:", "{stderr}");
    assert_eq!(lines[row + 4], "            allowRefs:", "{stderr}");
    assert_eq!(
        lines[row + 5],
        "                - \"tenants/cu/member-lookup.flow.nml\"",
        "{stderr}"
    );
    assert!(
        lines[row + 6].starts_with("for more information"),
        "{stderr}"
    );
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(code, 1);
    let d = rows
        .iter()
        .find(|r| r["code"] == "NML2064")
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert_eq!(
        d["suggestions"],
        serde_json::json!([{
            "kind": "insert",
            "source": "demo.package.nml",
            "edits": [{
                "line": 16, "col": 1, "endLine": 16, "endCol": 1,
                "lines": [
                    "        layers:",
                    "            allowRefs:",
                    "                - \"tenants/cu/member-lookup.flow.nml\"",
                    "",
                ],
            }],
        }]),
        "{d}"
    );
    let message = d["message"].as_str().unwrap();
    assert!(
        !message.contains('\n') && !message.contains("allowRefs"),
        "the wire's message is the one line it was, the block never in it: {d}"
    );
    // `-q` silences success lines and the explain hint, never a finding's
    // remedy: the block stays.
    let (code, _, quiet) = run(&[
        "check",
        "-q",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(code, 1, "{quiet}");
    assert!(
        quiet.contains("help: the block to add after line 15 of demo.package.nml")
            && quiet.contains("\n                - \"tenants/cu/member-lookup.flow.nml\"\n")
            && !quiet.contains("for more information"),
        "{quiet}"
    );
}

/// The block is RESOLVED against the manifest, never a canonical guess:
/// a two-space manifest gets a two-space block (its nested lines in the
/// file's own step), after ITS last line — and pasted as printed it is
/// the grant. The wire's edit says the same.
#[test]
fn the_remedy_block_follows_the_manifests_own_indentation() {
    let dir = workspace_copy("remedy-two-space");
    let root = dir.to_str().unwrap();
    let manifest = dir.join("demo.package.nml");
    let two_space = std::fs::read_to_string(&manifest)
        .unwrap()
        .lines()
        .map(|l| {
            let lead = l.len() - l.trim_start_matches(' ').len();
            format!("{}{}", " ".repeat(lead / 2), l.trim_start_matches(' '))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(
        two_space.contains("\n  - tenantFlows:\n    files:\n"),
        "{two_space}"
    );
    std::fs::write(&manifest, &two_space).unwrap();
    let file = dir.join("tenants/cu/member-lookup.flow.nml");
    let (code, _, stderr) = run(&["check", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(concat!(
            "help: the block to add after line 15 of demo.package.nml — paste it as printed:\n",
            "    layers:\n",
            "      allowRefs:\n",
            "        - \"tenants/cu/member-lookup.flow.nml\"\n"
        )),
        "{stderr}"
    );
    let (_, rows) = json_rows(&["check", "--json", "--root", root, file.to_str().unwrap()]);
    let d = rows
        .iter()
        .find(|r| r["code"] == "NML2064")
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert_eq!(
        d["suggestions"][0]["edits"][0]["lines"],
        serde_json::json!([
            "    layers:",
            "      allowRefs:",
            "        - \"tenants/cu/member-lookup.flow.nml\"",
            "",
        ]),
        "{d}"
    );
    let lines: Vec<&str> = stderr.lines().collect();
    let help = lines.iter().position(|l| l.starts_with("help: ")).unwrap();
    let block: Vec<&str> = lines[help + 1..]
        .iter()
        .take_while(|l| l.starts_with(' '))
        .copied()
        .collect();
    let last = "    strict = true\n";
    assert_eq!(two_space.matches(last).count(), 1, "{two_space}");
    std::fs::write(
        &manifest,
        two_space.replace(last, &format!("{last}{}\n", block.join("\n"))),
    )
    .unwrap();
    let (code, stdout, stderr) = run(&["check", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// The block's lines are CONTENT (spelled by the language's one string
/// speller, `nml_core::source_policy::string_literal` — the canonical
/// `\u{1B}` a diagnostic advises, never Rust's Debug `\u{1b}`): under the ASCII fold a key's own
/// glyph is spelled as the `\u{…}` escape the manifest decodes back to
/// it — never the tool's typography (an em dash folded to `--` would
/// paste as a different key, silently no grant) — and a key carrying a
/// control character or a bidi override is spelled as the escape on
/// every surface (stderr, the `--json` line), never raw. The folded
/// block, pasted as printed, is the grant.
#[test]
fn the_remedy_blocks_lines_are_content_on_every_surface() {
    let dir = workspace_copy("remedy-content");
    let root = dir.to_str().unwrap();
    let flow = std::fs::read_to_string(dir.join("tenants/cu/member-lookup.flow.nml")).unwrap();
    let dash = dir.join("tenants/cu/a\u{2014}b.flow.nml");
    std::fs::write(&dash, &flow).unwrap();
    let (code, _, folded) = run_env(
        &[("NML_UNICODE", Some("0"))],
        &["check", "--root", root, dash.to_str().unwrap()],
    );
    assert_eq!(code, 1, "{folded}");
    assert!(
        folded.contains("\n                - \"tenants/cu/a\\u{2014}b.flow.nml\"\n")
            && !folded.contains("- \"tenants/cu/a--b.flow.nml\""),
        "{folded}"
    );
    // The prose above it folds as prose does (its dash is the tool's).
    assert!(folded.contains(" -- paste it as printed:\n"), "{folded}");
    let lines: Vec<&str> = folded.lines().collect();
    let help = lines.iter().position(|l| l.starts_with("help: ")).unwrap();
    let block: Vec<&str> = lines[help + 1..]
        .iter()
        .take_while(|l| l.starts_with(' '))
        .copied()
        .collect();
    let manifest = dir.join("demo.package.nml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let last = "        strict = true\n";
    std::fs::write(
        &manifest,
        text.replace(last, &format!("{last}{}\n", block.join("\n"))),
    )
    .unwrap();
    let (code, stdout, stderr) = run_env(
        &[("NML_UNICODE", Some("0"))],
        &["check", "--root", root, dash.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");

    // A hostile key (on a fresh copy — the manifest above now grants):
    // the escape on every surface, the raw byte on none.
    let dir = workspace_copy("remedy-content-hostile");
    let root = dir.to_str().unwrap();
    let esc = dir.join("tenants/cu/e\u{1b}[31mv.flow.nml");
    std::fs::write(&esc, &flow).unwrap();
    let (code, _, stderr) = run(&["check", "--root", root, esc.to_str().unwrap()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("- \"tenants/cu/e\\u{1B}[31mv.flow.nml\"") && !stderr.contains('\x1b'),
        "{stderr}"
    );
    let (_, stdout, _) = run(&["check", "--json", "--root", root, esc.to_str().unwrap()]);
    assert!(!stdout.contains('\x1b'), "{stdout}");
    let row = rows_of(&stdout)
        .into_iter()
        .find(|r| r["code"] == "NML2064")
        .unwrap();
    assert_eq!(
        row["suggestions"][0]["edits"][0]["lines"][2],
        "                - \"tenants/cu/e\\u{1B}[31mv.flow.nml\"",
        "{row}"
    );
}

/// `nml fix` edits the file it was given, only: the remedy block's edit
/// lies in the manifest, so it is refused — printed, naming where it
/// goes — and never resolved against the content file's text, even when
/// a name in that file sits at the very byte span the manifest's binding
/// does (the anchor a stale-safe resolver relocates by span alone). The
/// manifest and the content file are byte-for-byte what they were.
#[test]
fn fix_never_applies_an_edit_that_lies_in_another_file() {
    let dir = workspace_copy("fix-elsewhere");
    let root = dir.to_str().unwrap();
    let manifest = dir.join("demo.package.nml");
    let manifest_text = std::fs::read_to_string(&manifest).unwrap();
    // The binding's name span in the manifest: `tenantFlows` at 152..163.
    let at = manifest_text.find("- tenantFlows:").unwrap() + 2;
    assert_eq!(at, 152, "{manifest_text}");
    // A content file whose `uses` block's name occupies exactly those
    // bytes: a resolver fed this text would find a named block there.
    let head = "thing base:\n    v = \"b\"\n\n// ";
    let name = "tenantFlows";
    let pad = at - head.len() - "\nthing ".len();
    let text = format!(
        "{head}{}\nthing {name} uses base:\n    v = \"t\"\n",
        "x".repeat(pad)
    );
    assert_eq!(text.find(name), Some(at), "{text}");
    let file = dir.join("tenants/cu/coincident.flow.nml");
    std::fs::write(&file, &text).unwrap();
    // Nothing applied, nothing failed: exit 0 (`fix --check` is the gate);
    // the refusal is printed, naming where the edit goes.
    let (code, stdout, stderr) = run(&["fix", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!(
            "{}:5:7: fix refused: the edit is in demo.package.nml — `nml fix` never edits another \
             file; take the did-you-mean or paste the `help:` block `nml check` shows there, or \
             apply the editor's quick fix\n",
            file.display()
        )),
        "{stderr}"
    );
    assert!(!stdout.contains("fixed"), "{stdout}");
    // The tally says a fix is pending ELSEWHERE, beside the standing count.
    assert!(
        stdout.contains(
            "0 edit(s) applied across 0 of 1 file(s); 1 diagnostic(s) not auto-fixable (1 \
             edit(s) belong to another file — pending there; `nml fix` never edits another file)"
        ),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_text);
    let (code, rows) = json_rows(&["fix", "--json", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{rows:?}");
    let refusal = rows
        .iter()
        .find(|r| r["severity"] == "info")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(
        refusal["message"],
        "fix refused: the edit is in demo.package.nml — `nml fix` never edits another file; take \
         the did-you-mean or paste the `help:` block `nml check` shows there, or apply the \
         editor's quick fix",
        "{refusal}"
    );
    assert_eq!(refusal["suggestions"], serde_json::json!([]), "{refusal}");
    // `routed` on the closing row: the one edit pending in the manifest,
    // beside `remaining` (which still counts the finding).
    let last = rows.last().unwrap();
    assert_eq!(last["routed"], 1, "{last}");
    assert_eq!(last["remaining"], 1, "{last}");
    assert_eq!(last["edits"], 0, "{last}");
    // The gate names it: an operator-side change is pending, distinct from
    // "an error no fix repairs".
    let (code, _, stderr) = run(&["fix", "--check", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("1 edit(s) belong to another file and are pending there"),
        "{stderr}"
    );
}

/// A REQUIRED own field named like the discriminator (`kind string`) is
/// NML2054 with its own consequence in the sentence — every instance
/// fails a missing-field error on a property it states — and the same
/// deletion as its fix; the optional shape keeps "a declaration nothing
/// can fill" (`shadowed-discriminator/own`).
#[test]
fn a_required_own_field_named_like_the_discriminator_says_no_instance_can_satisfy_it() {
    let dir = fixture("shadowed-discriminator/own-required");
    let (code, _stdout, stderr) = run_in(&dir, &["check", "--root", ".", "shadow.model.nml"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "shadow.model.nml:3:5: error[NML2054]: oneof 'record' arm \"log\": model 'logEntry' \
             declares a required field 'kind' named like the discriminator — an instance's \
             'kind' property is always read as the discriminator, so no instance can satisfy it \
             (a missing-field error on a property it states); delete it (to forbid arm \
             switching instead, seal it: `kind string? #sealed`)"
        ),
        "{stderr}"
    );
    let (code, stdout, _stderr) = run_in(
        &dir,
        &["fix", "--dry-run", "--root", ".", "shadow.model.nml"],
    );
    assert_eq!(code, 0, "{stdout}");
    // The field GOES — the deletion, not the seal's `?` (which would print
    // the same removed line beside a `+    kind string?`): no `kind` field
    // survives the fix on either side of the diff.
    assert!(stdout.contains("-    kind string\n"), "{stdout}");
    assert!(
        !stdout
            .lines()
            .any(|l| l.starts_with('+') && l.contains("kind string")),
        "the remedy is the deletion, not a re-spelling: {stdout}"
    );
}

/// `routed` counts DISTINCT edits, per file and on the per-file `fix`
/// row: two `uses` clauses in one file are two NML2064 findings carrying
/// ONE manifest edit (one binding to grant) — `routed` is 1 while
/// `remaining` is 2 — and a file whose own near-miss (`vv` in the plain
/// `base` block; a denied instance's body is not validated) IS fixed
/// gets its `fix` row with `applied` 1 and the routed edit beside it.
#[test]
fn routed_counts_distinct_edits_and_rides_the_per_file_row() {
    let dir = workspace_copy("fix-routed-distinct");
    let root = dir.to_str().unwrap();
    let file = dir.join("tenants/cu/two-uses.flow.nml");
    std::fs::write(
        &file,
        "thing base:\n    vv = \"b\"\n\nthing t uses base:\n    v = \"t\"\n\nthing u uses base:\n    v = \"u\"\n",
    )
    .unwrap();
    let (code, rows) = json_rows(&["fix", "--json", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{rows:?}");
    let per_file = rows
        .iter()
        .find(|r| r["type"] == "fix")
        .unwrap_or_else(|| panic!("the near-miss `vv` is this file's own fix: {rows:?}"));
    assert_eq!(per_file["applied"], 1, "{per_file}");
    assert_eq!(
        per_file["routed"], 1,
        "one manifest edit for two findings: {per_file}"
    );
    assert_eq!(per_file["remaining"], 2, "{per_file}");
    let last = rows.last().unwrap();
    assert_eq!(last["routed"], 1, "{last}");
    assert_eq!(last["remaining"], 2, "{last}");
    assert_eq!(last["edits"], 1, "{last}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .starts_with("thing base:\n    v = \"b\"\n"),
        "the near-miss was fixed in place"
    );
}

/// A `--schema` directory the run cannot read IN FULL — here a schema
/// source it cannot open — is the invocation's mistake in every verb
/// that takes the flag: exit 2, said once in the tool's words before
/// any target runs (an `error` row of kind `usage` under `--json`),
/// never a per-target failure and never a smaller schema universe under
/// a green gate (the directory's listing goes through the kernel's
/// rule, pinned in `workspace::schema_dir_tests`).
#[cfg(unix)]
#[test]
fn an_unreadable_schema_source_is_a_usage_error_before_any_target() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("schema-source-unreadable");
    std::fs::create_dir_all(dir.join("schemas")).unwrap();
    let locked = dir.join("schemas/locked.model.nml");
    std::fs::write(&locked, "model thing:\n    v string\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _unlock = Unlock(locked.clone());
    if std::fs::read_to_string(&locked).is_ok() {
        return; // root: the lock does not bite
    }
    for name in ["a.nml", "b.nml"] {
        std::fs::write(dir.join(name), "thing t:\n    v = \"x\"\n").unwrap();
    }
    let expected =
        "error: --schema schemas: cannot read schemas/locked.model.nml: permission denied\n";
    for verb in [vec!["check"], vec!["fix", "--check"]] {
        let mut args = verb.clone();
        args.extend(["--schema", "schemas", "--root", ".", "a.nml", "b.nml"]);
        let (code, stdout, stderr) = run_in(&dir, &args);
        assert_eq!(code, 2, "{verb:?}: {stdout}{stderr}");
        assert_eq!(stderr, expected, "{verb:?}: once, before any target");
        assert_eq!(stdout, "", "{verb:?}: no target ran: {stdout}");
        let mut json = vec![verb[0], "--json"];
        json.extend(args[1..].iter().copied());
        let out = nml_bin().current_dir(&dir).args(&json).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{json:?}");
        let first = &rows_of(&String::from_utf8_lossy(&out.stdout))[0];
        assert_eq!(first["type"], "error", "{first}");
        assert_eq!(first["kind"], "usage", "{first}");
    }
}

/// RFC 0026 B-8 (r94-sec): a FIFO named `*.model.nml` in a `--schema`
/// directory is the invocation's mistake — refused in the tool's words,
/// exit 2, once, before any target — never opened by path: the by-path
/// `read_to_string` blocked on it forever (a hang under a green gate)
/// while an unreadable entry was already refused. A link to a FIFO is
/// followed once (a link is read by path) and refused the same way.
#[cfg(unix)]
#[test]
fn a_fifo_named_as_a_schema_source_is_refused_never_blocked_on() {
    let dir = scratch_dir("schema-source-fifo");
    std::fs::create_dir_all(dir.join("schemas")).unwrap();
    std::fs::write(
        dir.join("schemas/a.model.nml"),
        "model thing:\n    v string\n",
    )
    .unwrap();
    std::fs::write(dir.join("t.nml"), "thing t:\n    v = \"x\"\n").unwrap();
    let pipe = dir.join("schemas/f.model.nml");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("mkfifo runs");
    assert!(status.success());
    std::os::unix::fs::symlink(&pipe, dir.join("schemas/l.model.nml")).unwrap();
    let schemas = dir.join("schemas");
    let expected = format!(
        "error: --schema {}: cannot read {}: not a regular file — a FIFO, socket or device is \
         never opened\n",
        schemas.display(),
        pipe.display()
    );
    for verb in [vec!["check"], vec!["fix", "--check"]] {
        let mut args = verb.clone();
        let root = dir.to_str().unwrap().to_string();
        let target = dir.join("t.nml").display().to_string();
        let schemas = schemas.to_str().unwrap().to_string();
        args.extend(["--schema", &schemas, "--root", &root, &target]);
        let started = std::time::Instant::now();
        let (code, stdout, stderr) = run_bounded(&args, 20);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{verb:?}"
        );
        assert_eq!(code, 2, "{verb:?}: {stdout}{stderr}");
        assert_eq!(stderr, expected, "{verb:?}: once, before any target");
        assert_eq!(stdout, "", "{verb:?}: no target ran");
    }
    // The FIFO gone, the LINK to it is next in listing order: followed
    // once, refused the same way, naming the link as typed.
    std::fs::remove_file(&pipe).unwrap();
    std::process::Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .expect("mkfifo runs");
    std::fs::remove_file(dir.join("schemas/l.model.nml")).unwrap();
    std::os::unix::fs::symlink(dir.join("pipe"), dir.join("schemas/l.model.nml")).unwrap();
    let root = dir.to_str().unwrap().to_string();
    let target = dir.join("t.nml").display().to_string();
    let schemas_arg = schemas.to_str().unwrap().to_string();
    let (code, _, stderr) = run_bounded(
        &["check", "--schema", &schemas_arg, "--root", &root, &target],
        20,
    );
    assert_eq!(code, 2, "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "error: --schema {}: cannot read {}: not a regular file — a FIFO, socket or device \
             is never opened
",
            schemas.display(),
            dir.join("schemas/l.model.nml").display()
        )
    );
}

/// A `--schema` source is read under the SAME bound a manifest-declared
/// source is read under (`MAX_SOURCE_BYTES`, 4 MiB): one file reached
/// through `--schema` and the same file reached through a manifest are
/// refused at the same size, in the kernel's one sentence. Before this
/// the `--schema` read was the last UNBOUNDED one in the tool — an
/// `open_beneath` followed by a bare `read_to_string`, so a 2 GB file
/// named `x.model.nml` was held whole while every other door refused it.
/// At the bound it loads; one byte past it the invocation is refused,
/// exit 2, once, before any target.
#[test]
fn a_schema_source_is_read_under_the_declared_source_bound() {
    const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
    let dir = scratch_dir("schema-source-bound");
    std::fs::create_dir_all(dir.join("schemas")).unwrap();
    std::fs::write(dir.join("t.nml"), "thing t:\n    v = \"x\"\n").unwrap();
    let source = dir.join("schemas/a.model.nml");
    let head = "model thing:\n    v string\n";
    for over in [false, true] {
        // Trailing blank lines: valid NML either way, so the two cases
        // differ in ONE byte.
        let pad = MAX_SOURCE_BYTES + usize::from(over) - head.len();
        std::fs::write(&source, format!("{head}{}", "\n".repeat(pad))).unwrap();
        let root = dir.to_str().unwrap().to_string();
        let schemas = dir.join("schemas").to_str().unwrap().to_string();
        let target = dir.join("t.nml").display().to_string();
        let (code, stdout, stderr) = run_bounded(
            &["check", "--schema", &schemas, "--root", &root, &target],
            60,
        );
        if over {
            assert_eq!(code, 2, "past the bound: {stdout}{stderr}");
            assert_eq!(
                stderr,
                format!(
                    // The size is rendered in the CAP's unit and marked
                    // `over` when it is not a whole one, so the two halves
                    // of the comparison are in one unit; the exact byte
                    // counts sit beside both.
                    "error: --schema {}: cannot read {}: too large: over 4 MiB (4194305 bytes) — \
                     a schema source is read only up to 4 MiB (4194304 bytes)\n",
                    dir.join("schemas").display(),
                    source.display()
                ),
                "the kernel's one sentence, once, before any target"
            );
            assert_eq!(stdout, "", "no target ran");
        } else {
            assert_eq!(code, 0, "at the bound: {stdout}{stderr}");
        }
    }
}

/// r88 P1: a binding whose declared source fails to load is the KERNEL's
/// row (NML2091) on the file, exit 1 in every verb — the universe's
/// content, not the invocation (it was `error: binding … cannot build its
/// validator: …`, a usage error, exit 2, and `binding` exited 0 fully
/// bound) — with the source's first finding as a `note:` line and a
/// `related[]` entry under `--json`; the manifest's other binding is
/// unaffected; the source document keeps its own findings.
#[test]
fn a_binding_whose_source_fails_to_load_is_the_kernels_row_in_every_verb() {
    let root = fixture("workspace-brokensrc");
    let root = root.to_str().unwrap();
    let tenant = format!("{root}/tenants/cu/plain.flow.nml");
    let row = "tenants/cu/plain.flow.nml: error[NML2091]: binding 'tenantFlows' of \
               demo.package.nml cannot build its validator: declared source `core` failed to \
               load at core.model.nml:3:1 (finding 1 of 5): indentation of 2 matches no \
               enclosing block (open \
               blocks are at columns 0, 4) — the file validates under no binding \
               until the source loads";
    let note = "core.model.nml:3:1: note: indentation of 2 matches no enclosing block (open \
                blocks are at columns 0, 4)";
    for verb in [
        vec!["check", "--root", root, &tenant],
        vec!["check", "--root", root, "--strict", &tenant],
        vec!["validate", "--root", root, &tenant],
        vec!["fix", "--check", "--root", root, &tenant],
    ] {
        let (code, stdout, stderr) = run(&verb);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        assert!(stderr.contains(row), "{verb:?}: {stderr}");
        assert!(stderr.contains(note), "{verb:?}: {stderr}");
        assert!(!stderr.contains("usage"), "{verb:?}: {stderr}");
    }
    // `binding`: the row rides `notes`, the exit is 1 — a binding stands.
    let (code, stdout, stderr) = run(&["binding", "--root", root, &tenant]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("binding   tenantFlows"), "{stdout}");
    assert!(stdout.contains(&format!("notes     {row}")), "{stdout}");
    // `--json`: the row's code and its located related note.
    let (code, stdout, _) = run(&["check", "--root", root, "--json", &tenant]);
    assert_eq!(code, 1);
    let rows = rows_of(&stdout);
    let diag = rows
        .iter()
        .find(|r| r["code"] == "NML2091")
        .unwrap_or_else(|| panic!("no NML2091 row: {stdout}"));
    assert_eq!(diag["source"], "tenants/cu/plain.flow.nml");
    assert_eq!(diag["related"][0]["source"], "core.model.nml");
    assert_eq!(diag["related"][0]["line"], 3);
    assert_eq!(diag["related"][0]["col"], 1);
    assert_eq!(rows.last().unwrap()["exit"], 1);
    // The sibling binding validates; the source document reports itself.
    let (code, stdout, _) = run(&[
        "check",
        "--root",
        root,
        &format!("{root}/docs/readme.doc.nml"),
    ]);
    assert_eq!(code, 0, "{stdout}");
    let (code, _, stderr) = run(&["check", "--root", root, &format!("{root}/core.model.nml")]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("error[NML0006]") && !stderr.contains("NML2091"),
        "{stderr}"
    );
}

/// `--strict` does not apply to a file a binding governs: the binding's
/// own `strict` is the file's strictness in every front end (the editor
/// has no flag to tighten it with — one verdict), so the CLI keeps the
/// binding's verdict (the unknown property stays a WARNING, exit 0
/// where the flag used to make it an error, exit 1) and says so ONCE
/// per run, naming the binding to set `strict = true` on — for the
/// first file under a lenient binding only (nothing for a strict
/// binding, where the flag changes nothing; nothing under `--json` or
/// `--quiet`). `--strict` with NOTHING to enforce stays the usage
/// error it was.
#[test]
fn strict_does_not_apply_to_a_bound_file_and_says_so_once_per_run() {
    let root = fixture("workspace-lenient");
    let root = root.to_str().unwrap();
    let extra = format!("{root}/tenants/cu/extra.flow.nml");
    let plain = format!("{root}/tenants/cu/plain.flow.nml");
    let note = "note: --strict does not apply to ";
    let (code, _, stderr) = run(&["check", "--root", root, "--strict", &extra]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("warning[NML2001]") && !stderr.contains("error[NML2001]"),
        "the binding's own verdict, not the flag's: {stderr}"
    );
    assert_eq!(stderr.matches(note).count(), 1, "{stderr}");
    assert!(
        stderr.contains(
            "binding 'tenantFlows' of demo.package.nml declares no `strict = true`; set it on \
             the binding to enforce it everywhere"
        ),
        "{stderr}"
    );
    // Two bound targets: said once, for the first.
    let (code, _, stderr) = run(&["check", "--root", root, "--strict", &extra, &plain]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr.matches(note).count(), 1, "{stderr}");
    assert!(
        stderr.contains("extra.flow.nml: a manifest-governed"),
        "{stderr}"
    );
    // Not under `--json` (a human note) nor `--quiet`; the row keeps its severity.
    let (code, stdout, stderr) = run(&["check", "--root", root, "--strict", "--json", &extra]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stderr.contains(note), "{stderr}");
    assert!(stdout.contains("\"severity\":\"warning\""), "{stdout}");
    let (_, _, stderr) = run(&["check", "--root", root, "--strict", "--quiet", &extra]);
    assert!(!stderr.contains(note), "{stderr}");
    // A STRICT binding: the flag changes nothing, so nothing is said.
    let strict_root = fixture("workspace");
    let strict_root = strict_root.to_str().unwrap();
    let (_, _, stderr) = run(&[
        "check",
        "--root",
        strict_root,
        "--strict",
        &format!("{strict_root}/tenants/cu/plain.flow.nml"),
    ]);
    assert!(!stderr.contains(note), "{stderr}");
}

/// A usage error exits 2 in EVERY verb — an unknown flag, a missing or
/// surplus positional, a flag without its value — and under `--json` is
/// an `error` row of kind `usage` carrying that exit; the domain
/// failures keep exit 1.
#[test]
fn usage_errors_exit_two_in_every_verb() {
    let dir = workspace_copy("usage-two");
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    for args in [
        vec!["check", "--bogus", &plain],
        vec!["check", "-x", &plain],
        vec!["check", "-qj", &plain],
        vec!["check", "--root=", &plain],
        // r86: the separate empty value (`--root "$ROOT"`, ROOT unset) is
        // the same mistake — it read as the working directory.
        vec!["check", "--root", "", &plain],
        vec!["binding", "--root", "", &plain],
        vec!["check", "--schema", "", &plain],
        // r87: a `--schema` directory that cannot be read is the same
        // mistake, checked before any target runs (`check` failed per
        // target with exit 1; `fix` degraded to the file alone, exit 0).
        vec!["check", "--schema", "/nonexistent-schema-dir-r87", &plain],
        vec![
            "fix",
            "--check",
            "--schema",
            "/nonexistent-schema-dir-r87",
            &plain,
        ],
        // r86 (mutant MR01 survived): an empty argument after `--` is
        // refused too.
        vec!["check", "--", ""],
        vec![
            "check",
            "--strict",
            "tests/fixtures/valid/minimal-service.nml",
        ],
        vec!["check"],
        vec!["check", "--root"],
        vec!["validate", "--strict", &plain],
        vec!["fix", "--check=yes", &plain],
        vec!["binding"],
        vec!["parse"],
        vec!["parse", &plain, &plain],
        vec!["parse", "--root", ".", &plain],
        vec!["fmt", "--bogus", &plain],
        vec!["explain"],
        vec!["explain", "--list", "NML2007"],
        vec!["limits", "extra"],
        vec!["limits", "--bogus"],
        // `version` is a verb like every other (r85 P1): a surplus
        // argument is refused.
        vec!["version", "extra"],
        vec!["version", "--bogus"],
        // An empty argument is no path at all (r85 F15): every verb.
        vec!["check", ""],
        vec!["validate", ""],
        vec!["fix", "--dry-run", ""],
        vec!["binding", ""],
        vec!["parse", ""],
        vec!["fmt", ""],
        vec!["explain", ""],
        // A target outside the root: the invocation names a universe
        // the file is not in (r85 D3) — every workspace verb.
        vec![
            "check",
            "--root",
            "tests/fixtures/workspace",
            "tests/fixtures/workspace-open/x.nml",
        ],
        vec![
            "validate",
            "--root",
            "tests/fixtures/workspace",
            "tests/fixtures/workspace-open/x.nml",
        ],
        vec![
            "fix",
            "--dry-run",
            "--root",
            "tests/fixtures/workspace",
            "tests/fixtures/workspace-open/x.nml",
        ],
        vec![
            "binding",
            "--root",
            "tests/fixtures/workspace",
            "tests/fixtures/workspace-open/x.nml",
        ],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 2, "{args:?}: {stderr}");
        assert!(stderr.starts_with("error: "), "{args:?}: {stderr}");
        let mut json = vec![args[0], "--json"];
        json.extend(args[1..].iter().copied());
        let (code, rows) = json_rows(&json);
        assert_eq!(code, 2, "{json:?}");
        assert_eq!(rows[0]["type"], "error", "{json:?}: {}", rows[0]);
        assert_eq!(rows[0]["kind"], "usage", "{json:?}: {}", rows[0]);
        assert_eq!(rows[0]["exit"], 2, "{json:?}: {}", rows[0]);
        assert_eq!(rows.last().unwrap()["exit"], 2, "{json:?}");
    }
    // Domain failures stay 1: an absent file, a file that does not
    // parse, an unknown explain code.
    let absent = dir.join("nope.nml").display().to_string();
    for args in [
        vec!["check", &absent],
        vec!["parse", &absent],
        vec!["explain", "NML9999"],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, 1, "{args:?}: {stderr}");
    }
}

/// A `key:` block dedented to a list body's item column — the shape a
/// remedy's block takes when pasted at the indentation it was printed
/// with — is NML0002 at the line for `parse`, `check` and `fmt` alike,
/// located on the wire, and `fmt` leaves the file byte-identical. It
/// used to lower to nothing: `parse` showed a tree without it, `check`
/// passed, and `fmt` rewrote the file without the three lines (content
/// loss against RFC 0004's lossless promise).
#[test]
fn fmt_refuses_a_block_dedented_to_the_item_column_and_leaves_the_file_untouched() {
    let dir = scratch_dir("fmt-dedented-block");
    let path = dir.join("m.nml");
    let text = "[]validator validators:\n    - a:\n        files:\n            - \"x/**\"\n    \
                stray:\n        allowRefs:\n            - \"y\"\n    - b:\n        files:\n            \
                - \"z/**\"\n";
    std::fs::write(&path, text).unwrap();
    let path = path.display().to_string();
    let line = format!(
        "{path}:5:5: error[NML0002]: expected a list item, a property, a modifier or a shared \
         property in an array body, found a nested block\n"
    );
    for verb in ["parse", "check", "fmt"] {
        let (code, stdout, stderr) = run(&[verb, &path]);
        assert_eq!(code, 1, "{verb}: {stderr}");
        assert!(stderr.contains(&line), "{verb}: want {line:?} in {stderr}");
        assert!(
            stderr.contains("error: 1 parse error(s)"),
            "{verb}: {stderr}"
        );
        assert!(!stdout.contains("formatted"), "{verb}: {stdout}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "untouched");
    let (code, rows) = json_rows(&["fmt", "--json", &path]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML0002")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["line"], 5, "{row}");
    assert_eq!(row["col"], 5, "{row}");
}

/// The language's inline array (`files = ["…"]`) is a manifest list like
/// the block form: it binds files, grants composition, declares budget
/// units — read through the one accessor the meta-schema's `[]string`
/// admits both spellings for — and a loader rule broken by an inline
/// element is located at that element. It used to load and be IGNORED:
/// `files` reported empty (a false sentence), `allowRefs` never read,
/// `budgetUnits` silently the inferred ones.
#[test]
fn inline_arrays_in_a_manifest_bind_grant_and_declare_like_block_lists() {
    let file = "tenants/cu/plain.flow.nml";
    let manifest = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
                    schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator \
                    validators:\n    - tenantFlows:\n        files = [\"tenants/**/*.flow.nml\"]\n        \
                    schemas = [core]\n        strict = true\n        layers:\n            allowRefs = \
                    [\"tenants/**\"]\n            denyRefs = [\"tenants/cu/vetoed/**\"]\n            \
                    maxStackDepth = 16.0\n";
    let dir = workspace_copy("inline-arrays");
    std::fs::write(dir.join("demo.package.nml"), manifest).unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stdout, stderr) = run_in(&dir, &["binding", "--root", ".", file]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    for want in [
        "binding   tenantFlows",
        "matched files[0] = \"tenants/**/*.flow.nml\"",
        "layers    granted",
        "allowRefs[0] = \"tenants/**\"",
        "denyRefs[0] = \"tenants/cu/vetoed/**\"",
        "maxStackDepth = 16",
    ] {
        assert!(stdout.contains(want), "want {want:?} in {stdout}");
    }
    // A loader rule broken by an inline element is located AT the element.
    let broken = manifest.replace(
        "allowRefs = [\"tenants/**\"]",
        "allowRefs = [\"tenants/**x\"]",
    );
    assert_ne!(broken, manifest);
    let at = broken.find("\"tenants/**x\"").unwrap();
    let line = broken[..at].matches('\n').count() + 1;
    let col = at - broken[..at].rfind('\n').map_or(0, |i| i + 1) + 1;
    std::fs::write(dir.join("demo.package.nml"), &broken).unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "demo.package.nml:{line}:{col}: error[NML2081]: manifest failed to load: validator \
             'tenantFlows' layers.allowRefs[0] = \"tenants/**x\": \
             `**` must be a whole segment\n"
        )),
        "{stderr}"
    );
    // The gap lint's remedy pastes as ONE line and is honoured.
    let dir = scratch_dir("inline-budget-units");
    copy_tree(&fixture("workspace-gap"), &dir);
    let manifest = std::fs::read_to_string(dir.join("demo.package.nml")).unwrap();
    let declared = manifest.replace(
        "    formatVersion = 1\n",
        "    formatVersion = 1\n    budgetUnits = [\"tenants/*\"]\n",
    );
    assert_ne!(declared, manifest);
    std::fs::write(dir.join("demo.package.nml"), declared).unwrap();
    let (code, _, stderr) = run_in(
        &dir,
        &["check", "--root", ".", "tenants/cu/flows/plain.flow.nml"],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("NML2092"),
        "declared units silence the lint: {stderr}"
    );
}

/// `fmt` reports a file that does not parse exactly as `parse` does —
/// EVERY parse error, each with its code, the same hint, the same
/// closing verdict (it is the same parser) — and writes nothing.
#[test]
fn fmt_reports_every_parse_error_with_its_code_like_parse() {
    let dir = scratch_dir("fmt-parse-errors");
    let broken = dir.join("broken.nml");
    let text = "thing a:\n    v ==\n\nthing b:\n    w ==\n";
    std::fs::write(&broken, text).unwrap();
    let broken = broken.display().to_string();
    let (code, _, from_parse) = run(&["parse", &broken]);
    assert_eq!(code, 1, "{from_parse}");
    let (code, stdout, from_fmt) = run(&["fmt", &broken]);
    assert_eq!(code, 1, "{from_fmt}");
    assert_eq!(stdout, "", "nothing formatted: {stdout}");
    assert_eq!(from_fmt, from_parse, "the same parser, the same report");
    assert!(
        from_fmt.matches("error[NML0002]").count() >= 2,
        "{from_fmt}"
    );
    assert!(
        from_fmt.contains("for more information, run: nml explain NML0002"),
        "{from_fmt}"
    );
    assert!(from_fmt.contains("parse error(s)"), "{from_fmt}");
    assert_eq!(std::fs::read_to_string(&broken).unwrap(), text, "untouched");
    let (code, rows) = json_rows(&["fmt", "--json", &broken]);
    assert_eq!(code, 1);
    assert!(
        rows.iter()
            .filter(|r| r["type"] == "diagnostic" && r["code"] == "NML0002")
            .count()
            >= 2,
        "{rows:#?}"
    );
    assert!(rows.iter().all(|r| r["type"] != "fmt"), "{rows:#?}");
}

/// The round trip: the `help:` snippet's lines, copied as printed (the
/// terminal's bytes, indentation included) and pasted after the
/// binding's last line, load as its grant — the next `check` passes and
/// `binding` prints the grant. A snippet that needed re-indenting would
/// land at the list's item column and silently be no grant.
fn the_help_snippet_pasted_as_printed_is_the_grant() {
    let dir = workspace_copy("help-roundtrip");
    let root = dir.to_str().unwrap();
    let file = dir.join("tenants/cu/member-lookup.flow.nml");
    let (code, _, stderr) = run(&["check", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 1, "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let help = lines
        .iter()
        .position(|l| l.starts_with("help: "))
        .unwrap_or_else(|| panic!("{stderr}"));
    let snippet: Vec<&str> = lines[help + 1..]
        .iter()
        .take_while(|l| l.starts_with(' '))
        .copied()
        .collect();
    assert_eq!(snippet.len(), 3, "{stderr}");
    let manifest = dir.join("demo.package.nml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let last = "        strict = true\n";
    assert_eq!(text.matches(last).count(), 1, "{text}");
    let pasted = text.replace(last, &format!("{last}{}\n", snippet.join("\n")));
    std::fs::write(&manifest, pasted).unwrap();
    let (code, stdout, stderr) = run(&["check", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stderr.contains("NML2064"), "{stderr}");
    let (code, stdout, _) = run(&["binding", "--root", root, file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains(
            "layers    granted\n          allowRefs[0] = \"tenants/cu/member-lookup.flow.nml\"\n"
        ),
        "{stdout}"
    );
}

/// The exit-code rows of the verbs that walk a universe name every code
/// that fails a run as a universe error — the manifest grant rule
/// (NML2081) beside the three the range names — and, on the walking
/// verbs, the skipped-content failure (NML2090).
#[test]
fn the_exit_code_rows_name_the_grant_rule_and_the_skipped_content_failure() {
    for verb in ["check", "validate", "binding"] {
        let (code, stdout, _) = run(&[verb, "--help"]);
        assert_eq!(code, 0, "{verb}: {stdout}");
        assert!(
            stdout.contains("(NML2081, NML2087–NML2089)"),
            "{verb}: {stdout}"
        );
    }
    for verb in ["check", "validate"] {
        let (_, stdout, _) = run(&[verb, "--help"]);
        assert!(
            stdout.contains("directory walk skipped (NML2090)"),
            "{verb}: {stdout}"
        );
    }
}

/// `binding` on a directory says so and exits 2 — never a `binding none`
/// block for a path that is not a file.
#[test]
fn binding_on_a_directory_says_so_and_exits_two() {
    let dir = workspace_copy("binding-dir");
    let root = dir.to_str().unwrap();
    for target in ["tenants/cu", "tenants/cu/", "tenants"] {
        let (code, stdout, stderr) = run(&[
            "binding",
            "--root",
            root,
            dir.join(target).to_str().unwrap(),
        ]);
        assert_eq!(code, 2, "{target}: {stdout}{stderr}");
        assert!(
            stderr.contains("is a directory — nml binding takes files; name a file under it"),
            "{target}: {stderr}"
        );
        assert!(!stdout.contains("binding   none"), "{target}: {stdout}");
    }
    let (code, rows) = json_rows(&[
        "binding",
        "--json",
        "--root",
        root,
        dir.join("tenants/cu").to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    assert_eq!(rows[0]["type"], "error");
    assert_eq!(rows[0]["kind"], "usage", "{}", rows[0]);
}

/// The checking verbs report their targets in ARGUMENT order, each
/// directory's expansion sorted within it — the first error names the
/// hint, and a `--json` consumer reads the rows in the order it asked.
#[test]
fn targets_are_reported_in_argument_order_with_each_directory_sorted() {
    let dir = workspace_copy("arg-order");
    let root = dir.to_str().unwrap();
    let vendor = dir.join("vendor").display().to_string();
    let cu = dir.join("tenants/cu").display().to_string();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    // r84-cov (mutant N13 survived): SORTED within the directory, not the
    // kernel's depth-then-key order — a nested `a/z.flow.nml` sorts before
    // the shallower `bad.flow.nml`.
    std::fs::create_dir_all(dir.join("tenants/cu/a")).unwrap();
    std::fs::copy(&plain, dir.join("tenants/cu/a/z.flow.nml")).unwrap();
    let (_, rows) = json_rows(&["validate", "--json", "--root", root, &vendor, &plain, &cu]);
    let targets: Vec<&str> = rows
        .iter()
        .filter(|r| r["type"] == "result")
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        targets,
        [
            "vendor/base.flow.nml",
            "tenants/cu/plain.flow.nml",
            "tenants/cu/a/z.flow.nml",
            "tenants/cu/bad.flow.nml",
            "tenants/cu/member-lookup.flow.nml",
            "tenants/cu/nml-project.nml",
            "tenants/cu/plain.flow.nml",
        ],
        "{rows:#?}"
    );
    assert_eq!(rows.last().unwrap()["targets"], 7, "{rows:#?}");
}

/// The `--json` vocabulary: every enum value in the stream is one
/// lowerCamel word — `root.origin` (`explicit`, `derivedVcsFence`, …),
/// `binding.step` (`pinned`, `autoAssociated`), `truncatedUnits[].why`,
/// `closure`, `universe`, `governing`, `severity`, `error.kind` — and
/// the printing budget is `withheld` (`truncated` names nothing).
#[test]
fn the_json_vocabulary_is_lowercamel_throughout() {
    let dir = workspace_copy("json-vocab");
    let root = dir.to_str().unwrap();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (_, rows) = json_rows(&["binding", "--json", "--root", root, &plain]);
    let b = &rows[0];
    assert_eq!(b["root"]["origin"], "explicit", "{b}");
    // A glob-claimed file is auto-associated (a `schemaPackages` pin
    // would be `pinned`).
    assert_eq!(b["binding"]["step"], "autoAssociated", "{b}");
    assert_eq!(b["binding"]["class"], "workspace", "{b}");
    assert_eq!(b["closure"], "complete", "{b}");
    assert_eq!(b["governing"], "bound", "{b}");
    let last = rows.last().unwrap();
    assert!(last.get("withheld").is_some(), "{last}");
    assert!(last.get("truncated").is_none(), "{last}");
    let word = |v: &serde_json::Value| {
        let s = v.as_str().unwrap();
        assert!(
            s.bytes().all(|c| c.is_ascii_alphanumeric())
                && s.starts_with(|c: char| c.is_ascii_lowercase()),
            "not a lowerCamel word: {s:?}"
        );
    };
    for v in [
        &b["root"]["origin"],
        &b["binding"]["step"],
        &b["binding"]["class"],
        &b["closure"],
        &b["governing"],
        &b["universe"],
        &last["closure"],
        &last["universe"],
    ] {
        word(v);
    }
    for note in b["notes"].as_array().unwrap() {
        word(&note["severity"]);
    }
    let (_, rows) = json_rows(&["check", "--json", "--root", root, "nope.nml"]);
    let error = rows.iter().find(|r| r["type"] == "error").unwrap();
    word(&error["kind"]);
}

/// Step 0f: on the wire a workspace file is named by its KEY — `source`
/// on a located finding, on a universe note and on a same-file
/// `related[]` entry alike, however the target was typed — while the
/// human `file:line:col` prefix keeps the path as typed; a foreign
/// note's file is read by its key through the root. A `--schema`
/// source, no workspace file, keeps its basename.
#[test]
fn json_source_is_the_workspace_key_however_the_target_was_typed() {
    let dir = workspace_copy("keyed-source");
    let root = dir.to_str().unwrap();
    let parent = dir.as_ref().parent().unwrap().to_path_buf();
    let proj = dir
        .as_ref()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // Typed from the parent, through `..`: the finding's `source` is the
    // key; the human line keeps the spelling.
    let typed = format!("{proj}/vendor/../tenants/cu/bad.flow.nml");
    let (code, stdout, stderr) = run_in(&parent, &["check", "--json", "--root", &proj, &typed]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let rows = rows_of(&stdout);
    let finding = rows
        .iter()
        .find(|r| r["type"] == "diagnostic" && r["code"] == "NML2008")
        .expect("the file's own finding");
    assert_eq!(finding["source"], "tenants/cu/bad.flow.nml", "{finding}");
    let result = rows.iter().find(|r| r["type"] == "result").expect("result");
    assert_eq!(result["key"], "tenants/cu/bad.flow.nml", "{result}");
    assert_eq!(result["target"], typed, "as typed: {result}");
    let (code, _, stderr) = run_in(&parent, &["check", "--root", &proj, &typed]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains(&format!("{typed}:2:9: error[NML2008]")),
        "the human prefix is the path as typed: {stderr}"
    );
    // A related note names the key of the file its span indexes — the
    // composing file's own for a same-file note, the MANIFEST's for
    // NML2064's located remedy (RFC 0026 B-1: the note sits at the
    // binding in `demo.package.nml`) — never an absolute path or the
    // typed spelling.
    let composed = dir.join("tenants/cu/member-lookup.flow.nml");
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        root,
        composed.to_str().unwrap(),
    ]);
    let mut located_remedy = false;
    for row in rows.iter().filter(|r| r["type"] == "diagnostic") {
        for rel in row["related"].as_array().unwrap() {
            if row["code"] == "NML2064" && rel["source"] == "demo.package.nml" {
                located_remedy = true;
                continue;
            }
            assert_eq!(
                rel["source"], row["source"],
                "same-file notes name the key: {row}"
            );
        }
    }
    assert!(
        located_remedy,
        "the no-grant remedy is a note in the manifest: {rows:?}"
    );
    // A `--schema` source keeps its basename: not a workspace file.
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--schema",
        dir.join("vendor").to_str().unwrap(),
        dir.join("docs/unclaimed.nml").to_str().unwrap(),
    ]);
    for row in rows.iter().filter(|r| r["type"] == "diagnostic") {
        let source = row["source"].as_str().unwrap();
        assert!(
            !source.starts_with('/'),
            "never absolute on the wire: {row}"
        );
    }
}

/// r80-sec F4: the CI gate over content the walk skipped. In the
/// probe's layout — a symlinked `*.flow.nml`, a FIFO, a `.hidden`
/// directory holding a `.flow.nml`, a `.secret.flow.nml` dot-file and a
/// symlinked directory under a closed universe — `fix --check --root . .`
/// exited 0 ("0 of 3 file(s)") while the same link, named, was
/// NML2083. Now every walking verb with a directory target reports the
/// skipped `.nml` content (the link in the resolver's NML2083 words, the
/// rest NML2090), the symlinked directory as a warning, fails, and the
/// closing row lists what the walk skipped; a FILE target gates nothing.
#[cfg(unix)]
#[test]
fn the_gate_fails_on_nml_content_the_walk_skipped() {
    let dir = workspace_copy("gate-skips");
    let root = dir.to_str().unwrap().to_string();
    let cu = dir.join("tenants/cu");
    std::os::unix::fs::symlink("../../outside.flow.nml", cu.join("link.flow.nml")).unwrap();
    std::os::unix::fs::symlink("../../vendor", cu.join("lib")).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(cu.join("fifo.flow.nml"))
            .status()
            .unwrap()
            .success()
    );
    std::fs::create_dir_all(cu.join(".hidden/deep")).unwrap();
    std::fs::write(cu.join(".hidden/bad.flow.nml"), "thing t:\n    v = 1\n").unwrap();
    std::fs::write(cu.join(".hidden/deep/deeper.flow.nml"), "").unwrap();
    std::fs::write(cu.join(".secret.flow.nml"), "").unwrap();
    let expected_rows = [
        "tenants/cu/link.flow.nml: error[NML2083]: closed binding rejects `tenants/cu/link.flow.nml`: path component `link.flow.nml` is a symlink",
        "tenants/cu/fifo.flow.nml: error[NML2090]: the walk skipped `tenants/cu/fifo.flow.nml`: a FIFO, socket or device",
        "tenants/cu/.secret.flow.nml: error[NML2090]: the walk skipped `tenants/cu/.secret.flow.nml`: a dot-file",
        // ONE row per hidden directory (r85, r84-sec F1): the directory,
        // the exact count, the keys — never a row per file.
        "tenants/cu/.hidden: error[NML2090]: the walk skipped `tenants/cu/.hidden`: a dot-directory it never enters, holding 2 `.nml` file(s) no verb judged (`tenants/cu/.hidden/bad.flow.nml`, `tenants/cu/.hidden/deep/deeper.flow.nml`) — content a runtime could read; move it where the walk lists it, or name the files on the command line",
        "tenants/cu/lib: warning[NML2090]: the walk skipped `tenants/cu/lib`: a symlink it did not enter",
    ];
    for verb in [&["fix", "--check"][..], &["check"][..], &["validate"][..]] {
        let mut args: Vec<&str> = verb.to_vec();
        args.extend(["--root", &root, &root]);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        for row in expected_rows {
            assert!(stderr.contains(row), "{verb:?} lacks {row}: {stderr}");
        }
        // `fix` fails first on the fixture's own refused path (NML2087,
        // "1 path(s) could not be fixed"); the checking verbs close on
        // the gate's count.
        if verb[0] != "fix" {
            assert!(
                stderr.contains("4 skipped path(s) hold content no verb judged"),
                "{verb:?}: {stderr}"
            );
        }
        assert!(
            !stderr.contains("deeper.flow.nml: error"),
            "a row per hidden file: {verb:?}: {stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
    // The gate's closing row: `skipped` names every row the walk left,
    // `errors` counts the gate's rows.
    let (code, rows) = json_rows(&["fix", "--check", "--json", "--root", &root, &root]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["exit"], 1, "{last}");
    assert!(last["errors"].as_u64().unwrap() >= 4, "{last}");
    let skipped: Vec<(String, String)> = last["skipped"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["key"].as_str().unwrap().to_string(),
                r["why"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for want in [
        ("tenants/cu/link.flow.nml", "symlink"),
        ("tenants/cu/lib", "symlink"),
        ("tenants/cu/fifo.flow.nml", "fifo"),
        ("tenants/cu/.hidden", "dotDirectory"),
        ("tenants/cu/.secret.flow.nml", "dotFile"),
    ] {
        assert!(
            skipped.contains(&(want.0.to_string(), want.1.to_string())),
            "{want:?} missing from {skipped:?}"
        );
    }
    assert!(
        rows.iter()
            .any(|r| r["code"] == "NML2083" && r["source"] == "tenants/cu/link.flow.nml"),
        "{rows:?}"
    );
    // A file target gates nothing: the file itself is judged.
    let plain = cu.join("plain.flow.nml").display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &plain]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("NML2090") && !stderr.contains("NML2083"),
        "{stderr}"
    );
    // `fix` without `--check` is not the gate: the rows ride the closing
    // row only, and the walk's own files are fixed (`tenants`: no
    // refused path under it, unlike the fixture's ambiguous `shared/`).
    let tenants = dir.join("tenants").display().to_string();
    let (code, _, stderr) = run(&["fix", "--dry-run", "--root", &root, &tenants]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("NML2090") && !stderr.contains("NML2083"),
        "{stderr}"
    );
    let (code, rows) = json_rows(&["fix", "--dry-run", "--json", "--root", &root, &tenants]);
    assert_eq!(code, 0);
    assert!(
        rows.last().unwrap()["skipped"]["shown"].as_u64().unwrap() >= 5,
        "{rows:?}"
    );
    // The gate on a directory with NOTHING else wrong — no refused path,
    // no pending edit, no error — fails on the skipped content alone.
    std::fs::create_dir_all(dir.join("tenants/only")).unwrap();
    std::fs::copy(
        cu.join("plain.flow.nml"),
        dir.join("tenants/only/plain.flow.nml"),
    )
    .unwrap();
    std::os::unix::fs::symlink("plain.flow.nml", dir.join("tenants/only/l.flow.nml")).unwrap();
    let only = dir.join("tenants/only").display().to_string();
    let (code, stdout, stderr) = run(&["fix", "--check", "--root", &root, &only]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr
            .contains("error: 1 skipped path(s) hold content no verb judged; nothing was written"),
        "{stderr}"
    );
    assert!(
        stdout.contains("0 edit(s) would apply across 0 of 1 file(s)"),
        "{stdout}"
    );
    // A directory target UNDER which nothing was skipped gates nothing.
    std::fs::create_dir_all(dir.join("tenants/clean")).unwrap();
    std::fs::copy(
        cu.join("plain.flow.nml"),
        dir.join("tenants/clean/plain.flow.nml"),
    )
    .unwrap();
    let clean = dir.join("tenants/clean").display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &clean]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("NML2090"), "{stderr}");
}

/// A directory at the 64-component bound is never listed — an exact
/// skip, never a truncation — and used to be a SILENT one: 62 nested
/// directories under `tenants/only` hid a tenant's `.flow.nml` from
/// `nml check --root . <dir>` (exit 0, `skipped: {}`) while naming the
/// file was refused (`more than 64 path components`). Now the gate
/// fails on the directory at its own key (`componentBound` on the
/// closing row); the sibling file still validates.
#[cfg(unix)]
#[test]
fn the_gate_fails_on_a_directory_at_the_component_bound() {
    let dir = workspace_copy("gate-depth");
    let root = dir.to_str().unwrap().to_string();
    let mut deep = std::path::PathBuf::from("tenants/only");
    for i in 0..62 {
        deep.push(format!("d{i}"));
    }
    // 64 components: the bound itself — nothing beneath it is keyable.
    assert_eq!(deep.components().count(), 64);
    std::fs::create_dir_all(dir.join(&deep)).unwrap();
    std::fs::write(
        dir.join(&deep).join("hidden.flow.nml"),
        "thing t:\n    v = 1\n",
    )
    .unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        dir.join("tenants/only/plain.flow.nml"),
    )
    .unwrap();
    let target = dir.join("tenants/only").display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let key = deep.display().to_string();
    let row = format!(
        "{key}: error[NML2090]: the walk skipped `{key}`: a directory at the 64-component bound \
         the walk never enters (nothing beneath it is keyable) — content a runtime could read \
         that no verb judged; flatten the tree, or move its content where the walk lists it"
    );
    assert!(stderr.contains(&row), "{stderr}");
    assert!(
        stderr.contains("1 skipped path(s) hold content no verb judged"),
        "{stderr}"
    );
    assert!(stdout.contains("plain.flow.nml: ok"), "{stdout}");
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["skipped"]["byWhy"]["componentBound"], 1, "{last}");
    assert!(
        last["skipped"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["key"] == key && r["why"] == "componentBound"),
        "{last}"
    );
    // `entry` rides an `unkeyableName` row only (the name inside that
    // reason, RFC 0026 B-2): a `componentBound` row never carries one.
    assert!(
        last["skipped"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["why"] == "componentBound")
            .all(|r| r.get("entry").is_none()),
        "{last}"
    );
}

/// r92-sec F1: an entry whose NAME no key can carry (`ev\il` — a `\` is
/// a legal byte in a unix file name, and git tracks it) used to be
/// charged and dropped in silence: `nml check --root . .` said `ok`,
/// exit 0, over a tenant's `ev\il/hidden.flow.nml` under the operator's
/// glob — a directory of content the walk never entered, certified. Now
/// the gate fails on it, under the holding directory, naming the entry
/// and what it is; the closing row counts it (`unkeyableName`) with the
/// name in `entry`; a `.txt` with the same defect is no content and no
/// row; a FILE target still gates nothing.
/// A `denyRefs` veto spelled with a template (`"tenants/cu/{{x}}"`) used to
/// LOAD as no veto at all — the meta-schema admits a template as a string,
/// the list reader set it aside — and the vetoed file composed `ok`
/// (fail-open). The manifest is refused at the element now, through the
/// universe's NML2088 row located there, before any glob is read, and
/// the file validates under no binding.
/// RFC 0026 decision 6: `fix` refused at the door said the finding and
/// then `error: N error(s)` — no tally, no "nothing was written", and no
/// word that the edit the finding's own did-you-mean names is one this run
/// will not make. It is the fourth surface the routed sentence promises
/// (the per-file refusal, the tally's note and the `--check` reason are
/// the other three), and the `--json` closing row already carried the
/// number. Silent under `-q`, as every closing tally is.
#[test]
fn fix_refused_at_the_door_says_its_verdict_and_where_the_edit_is_pending() {
    let fixture = fixture("manifest-rules/did-you-mean");
    let root = fixture.to_str().unwrap().to_string();
    let target = fixture
        .join("tenants/cu/plain.flow.nml")
        .display()
        .to_string();
    let verdict = "nothing was written: the universe validates and rewrites nothing until \
                   the error(s) above are repaired (1 edit(s) those findings carry are \
                   pending in the universe's own inputs — take the did-you-mean or paste \
                   the `help:` block `nml check` shows there, or apply the editor's quick fix)";
    for args in [
        vec!["fix", "--root", &root, &target],
        vec!["fix", "--dry-run", "--root", &root, &target],
        vec!["fix", "--check", "--root", &root, &target],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        assert!(stdout.contains(verdict), "{args:?}: {stdout}");
        assert!(stderr.contains("error[NML2088]"), "{args:?}: {stderr}");
    }
    // `-q` is errors only.
    let (code, stdout, _stderr) = run(&["fix", "-q", "--root", &root, &target]);
    assert_eq!(code, 1);
    assert_eq!(stdout, "", "quiet: {stdout}");
    // The same fact as a number, unchanged: nothing applied, one routed.
    let (_, rows) = json_rows(&["fix", "--json", "--root", &root, &target]);
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary", "{last}");
    assert_eq!(last["edits"], 0, "{last}");
    assert_eq!(last["routed"], 1, "{last}");
    // The verdict never rides the machine stream.
    let out = nml_bin()
        .args(["fix", "--json", "--root", &root, &target])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("nothing was written"),
        "--json carries rows, never prose"
    );
}

/// The verdict points at rows, so it prints only where rows stand. The
/// door also refuses a target naming no `.nml` file, and there nothing was
/// printed above it: "the universe validates and rewrites nothing until the
/// error(s) above are repaired" then blamed findings the reader never saw,
/// for a directory whose only fault is being empty. The `error:` line is
/// the whole story there.
#[test]
fn fix_refused_for_want_of_a_file_blames_no_findings_the_reader_cannot_see() {
    let dir = scratch_dir("r103-empty-fix");
    let target = dir.join("empty");
    std::fs::create_dir_all(&target).unwrap();
    let target = target.display().to_string();
    for args in [
        vec!["fix", &target],
        vec!["fix", "--dry-run", &target],
        vec!["fix", "--check", &target],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains("no .nml files found"), "{args:?}: {stderr}");
        assert!(
            !stdout.contains("nothing was written"),
            "{args:?}: a verdict about findings above, with none above: {stdout}"
        );
    }
    // A door refusal that DOES print rows keeps its verdict.
    let fixture = fixture("manifest-rules/did-you-mean");
    let root = fixture.to_str().unwrap().to_string();
    let governed = fixture
        .join("tenants/cu/plain.flow.nml")
        .display()
        .to_string();
    let (code, stdout, stderr) = run(&["fix", "--root", &root, &governed]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("nothing was written"), "{stdout}");
}

/// RFC 0026 decision 6: a load error's finding COUNT qualifies the failure,
/// ahead of the colon that introduces the first finding — never after it,
/// where the renderer appends a did-you-mean and the count read as the
/// thing the hint repaired. One builder makes it, for all four sentences.
#[test]
fn a_load_errors_finding_count_reads_as_the_manifests_and_the_hint_closes_the_finding() {
    let dir = fixture("manifest-rules/did-you-mean");
    let root = dir.to_str().unwrap().to_string();
    let target = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (_, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    let message = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .and_then(|r| r["message"].as_str().map(str::to_string))
        .expect("the manifest row");
    assert!(!message.contains("more)"), "no tail survives: {message}");
    let count = message
        .find("(finding 1 of 2)")
        .unwrap_or_else(|| panic!("the count: {message}"));
    let finding = message
        .find("unknown property 'versio'")
        .unwrap_or_else(|| panic!("the finding: {message}"));
    assert!(
        count < finding,
        "the count qualifies the failure: {message}"
    );
    assert!(
        message.ends_with("(did you mean \"version\"?)"),
        "the hint closes the finding it repairs: {message}"
    );
    // A ONE-finding manifest carries no count at all.
    let one_finding = fixture("manifest-rules/no-schema");
    let one_finding = one_finding.to_str().unwrap().to_string();
    let (_, rows) = json_rows(&["check", "--json", "--root", &one_finding, &one_finding]);
    for row in &rows {
        if let Some(m) = row["message"].as_str() {
            assert!(!m.contains("(finding 1 of"), "{m}");
        }
    }
}

/// RFC 0026 decision 5: a `--schema <dir>` holding no schema source
/// certified a run that validated nothing — `ok`, exit 0, and no way to
/// tell it from a real validation. It is not a mistake by itself (RFC
/// 0012's self-validating file carries its own `model`), so it is
/// DISCLOSED: one `note:` line before any target, `schemaSources` on the
/// closing row, exit codes untouched — and `-q` (errors only) is silent,
/// as every other note is.
#[test]
fn a_source_less_schema_directory_is_disclosed_never_refused() {
    let dir = scratch_dir("schema-dir-empty");
    std::fs::create_dir_all(dir.join("schemas")).unwrap();
    std::fs::create_dir_all(dir.join("t")).unwrap();
    // RFC 0012's self-validating file: it carries its own `model`, so a
    // source-less `--schema` directory is a legitimate invocation.
    std::fs::write(
        dir.join("t/self.nml"),
        "model thing:\n    v string\n\nthing a:\n    v = \"x\"\n",
    )
    .unwrap();
    let note = "note: --schema schemas holds no schema source (.model.nml, .schema.nml): \
                every target is validated against its own definitions only";

    let (code, stdout, stderr) = run_in(
        &dir,
        &["check", "--schema", "schemas", "--root", ".", "t/self.nml"],
    );
    assert_eq!(code, 0, "the run is not refused: {stdout}{stderr}");
    assert!(stderr.contains(note), "{stderr}");
    assert!(stdout.contains("t/self.nml: ok"), "{stdout}");

    // `-q` is errors only.
    let (code, _stdout, stderr) = run_in(
        &dir,
        &[
            "check",
            "-q",
            "--schema",
            "schemas",
            "--root",
            ".",
            "t/self.nml",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "", "quiet: {stderr}");

    // The same fact as a number, for a consumer that reads no prose.
    let out = nml_bin()
        .current_dir(&dir)
        .args([
            "check",
            "--json",
            "--schema",
            "schemas",
            "--root",
            ".",
            "t/self.nml",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "the note never rides stdout's stream"
    );
    let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
    let last = rows.last().unwrap();
    assert_eq!(last["type"], "summary", "{last}");
    assert_eq!(last["schemaSources"], 0, "{last}");
    assert_eq!(last["exit"], 0, "{last}");

    // A directory that DOES hold one: no note, and the count says so.
    std::fs::write(
        dir.join("schemas/core.model.nml"),
        "model thing:\n    v string\n",
    )
    .unwrap();
    std::fs::write(dir.join("t/inst.nml"), "thing a:\n    v = \"x\"\n").unwrap();
    let (code, _stdout, stderr) = run_in(
        &dir,
        &["check", "--schema", "schemas", "--root", ".", "t/inst.nml"],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("holds no schema source"), "{stderr}");
    let out = nml_bin()
        .current_dir(&dir)
        .args([
            "check",
            "--json",
            "--schema",
            "schemas",
            "--root",
            ".",
            "t/inst.nml",
        ])
        .output()
        .unwrap();
    let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(
        rows.last().unwrap()["schemaSources"],
        1,
        "{:?}",
        rows.last()
    );

    // A run with no `--schema` carries no count at all.
    let out = nml_bin()
        .current_dir(&dir)
        .args(["check", "--json", "--root", ".", "t/inst.nml"])
        .output()
        .unwrap();
    let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
    assert!(
        rows.last().unwrap()["schemaSources"].is_null(),
        "{:?}",
        rows.last()
    );
}

/// RFC 0026 decision 1: a `[]schema` entry declaring a file spelled
/// outside the one admission (`*.model.nml`, `*.schema.nml`) is refused
/// AT LOAD — the universe's NML2088 row at the `file` value, NML2105 its
/// cause — so the CLI no longer judges a file the editor cannot see.
#[test]
fn a_declared_schema_source_not_spelled_as_one_is_refused_at_load() {
    let fixture = fixture("manifest-rules/schema-source-name");
    let root = fixture.to_str().unwrap().to_string();
    let target = fixture
        .join("tenants/cu/plain.flow.nml")
        .display()
        .to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.contains("error[NML2088]"), "{stderr}");
    assert!(
        stderr.contains(
            "`[]schema` entry 'core' declares \"core.nml\", which is not spelled as a schema \
             source (.model.nml, .schema.nml)"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains(": ok ("), "nothing composes: {stderr}");
    // The source itself is no longer judged as a schema: the universe it
    // belongs to does not load, so no verb reads its directives.
    let source = fixture.join("core.nml").display().to_string();
    let (code, _stdout, stderr) = run(&["check", "--root", &root, &source]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2088]"), "{stderr}");
    let (_, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .expect("the manifest row");
    // Located at the `file` VALUE, and the rule rides as the row's cause.
    assert_eq!(row["line"], 7, "{row}");
    assert_eq!(row["col"], 16, "{row}");
    assert_eq!(row["source"], "demo.package.nml", "{row}");
    assert_eq!(row["cause"]["code"], "NML2105", "{row}");
    assert_eq!(row["cause"]["line"], 7, "{row}");
    assert_eq!(row["cause"]["col"], 16, "{row}");
}

#[test]
fn a_manifest_list_element_spelled_as_a_template_is_refused_never_dropped() {
    let dir = workspace_copy("template-veto");
    let root = dir.to_str().unwrap().to_string();
    let manifest = dir.join("demo.package.nml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let with_veto = text.replace(
        "        strict = true\n",
        "        strict = true\n        layers:\n            allowRefs:\n                - \"**\"\n            \
         denyRefs:\n                - \"tenants/cu/{{x}}\"\n",
    );
    assert_ne!(with_veto, text);
    std::fs::write(&manifest, &with_veto).unwrap();
    let target = dir
        .join("tenants/cu/member-lookup.flow.nml")
        .display()
        .to_string();
    let (code, _stdout, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("error[NML2088]"), "{stderr}");
    assert!(
        stderr.contains("`denyRefs` holds a template string (`{{…}}`)"),
        "{stderr}"
    );
    assert!(!stderr.contains(": ok ("), "nothing composes: {stderr}");
    let (_, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .expect("the manifest row");
    // Located at the element: the veto's line and its opening quote.
    let line = with_veto.lines().position(|l| l.contains("{{x}}")).unwrap() + 1;
    assert_eq!(row["line"], line, "{row}");
    assert_eq!(row["col"], 19, "{row}");
    assert_eq!(row["source"], "demo.package.nml", "{row}");
    // The rule under its own code (NML2104), the row's cause.
    assert_eq!(row["cause"]["code"], "NML2104", "{row}");
}

#[cfg(unix)]
#[test]
fn the_gate_fails_on_an_entry_whose_name_no_key_can_carry() {
    let dir = workspace_copy("gate-unkeyable");
    let root = dir.to_str().unwrap().to_string();
    let only = dir.join("tenants/only");
    std::fs::create_dir_all(only.join("ev\\il")).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        only.join("plain.flow.nml"),
    )
    .unwrap();
    std::fs::write(only.join("ev\\il/hidden.flow.nml"), "thing t:\n    v = 1\n").unwrap();
    std::fs::write(only.join("ba\\d.flow.nml"), "thing t:\n    v = 1\n").unwrap();
    std::fs::write(only.join("no\\te.txt"), "").unwrap();
    std::os::unix::fs::symlink("plain.flow.nml", only.join("li\\nk")).unwrap();
    let target = only.display().to_string();
    let expected_rows = [
        "tenants/only: error[NML2090]: the walk skipped an entry under `tenants/only` whose name no key can carry (`ev\\il`: not UTF-8, or bearing a path separator): a directory the walk never entered",
        "tenants/only: error[NML2090]: the walk skipped an entry under `tenants/only` whose name no key can carry (`ba\\d.flow.nml`: not UTF-8, or bearing a path separator): a `.nml` file no verb judged",
        "tenants/only: error[NML2090]: the walk skipped an entry under `tenants/only` whose name no key can carry (`li\\nk`: not UTF-8, or bearing a path separator): a symlink it never entered nor read",
    ];
    for verb in [&["check"][..], &["validate"][..], &["fix", "--check"][..]] {
        let mut args: Vec<&str> = verb.to_vec();
        args.extend(["--root", &root, &target]);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        for row in expected_rows {
            assert!(stderr.contains(row), "{verb:?} lacks {row}: {stderr}");
        }
        assert!(!stderr.contains("no\\te.txt"), "{verb:?}: {stderr}");
        assert!(
            stderr.contains("3 skipped path(s) hold content no verb judged"),
            "{verb:?}: {stderr}"
        );
    }
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    assert_eq!(code, 1);
    let last = rows.last().unwrap();
    assert_eq!(last["skipped"]["byWhy"]["unkeyableName"], 3, "{last}");
    let skipped = last["skipped"]["rows"].as_array().unwrap();
    for entry in ["ev\\il", "ba\\d.flow.nml", "li\\nk"] {
        assert!(
            skipped.iter().any(|r| r["key"] == "tenants/only"
                && r["why"] == "unkeyableName"
                && r["entry"] == entry),
            "{entry} missing from {skipped:?}"
        );
    }
    assert_eq!(
        rows.iter().filter(|r| r["code"] == "NML2090").count(),
        3,
        "{rows:?}"
    );
    // A file target gates nothing: the file itself is judged.
    let plain = only.join("plain.flow.nml").display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &plain]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("NML2090"), "{stderr}");
}

/// The open-universe half: a symlinked `.nml` a directory walk left is
/// NML2090 (a link is followed only when named), never NML2083 (no
/// closed binding rejects anything here).
#[cfg(unix)]
#[test]
fn the_gate_names_a_walked_symlink_in_an_open_universe() {
    let dir = scratch_dir("gate-open");
    std::fs::write(dir.join("a.flow.nml"), "thing t:\n    v = 1\n").unwrap();
    std::os::unix::fs::symlink("a.flow.nml", dir.join("l.flow.nml")).unwrap();
    let root = dir.to_str().unwrap().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &root]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "l.flow.nml: error[NML2090]: the walk skipped `l.flow.nml`: a symlink — followed only \
             when named on the command line, never by a directory walk"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("NML2083"), "{stderr}");
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &root]);
    assert_eq!(code, 1);
    assert_eq!(rows.last().unwrap()["universe"], "open");
    assert_eq!(
        rows.last().unwrap()["skipped"],
        serde_json::json!({
            "byWhy": {"symlink": 1},
            "rows": [{"key": "l.flow.nml", "why": "symlink"}],
            "shown": 1,
            "hidden": 0,
        })
    );
}

/// The guide's CI line — `nml check --root . tenants/` — fails on skipped
/// `.nml` content ALONE: a directory whose only defect is a symlinked
/// `.nml` exits 1 under `check` and `validate` (the gate's count reaches
/// `gated()` for the checking verbs, not only `fix --check`; a mutant
/// that dropped it from `gated()` survived every pin).
#[cfg(unix)]
#[test]
fn check_and_validate_fail_on_skipped_content_alone() {
    let dir = scratch_dir("gate-alone");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    let clean = dir.join("tenants/clean");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::copy(
        fixture("workspace").join("tenants/cu/plain.flow.nml"),
        clean.join("plain.flow.nml"),
    )
    .unwrap();
    std::os::unix::fs::symlink("plain.flow.nml", clean.join("l.flow.nml")).unwrap();
    let root = dir.display().to_string();
    let target = clean.display().to_string();
    for verb in ["check", "validate"] {
        let (code, stdout, stderr) = run(&[verb, "--root", &root, &target]);
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(stdout.contains("plain.flow.nml: ok"), "{verb}: {stdout}");
        assert!(
            stderr.contains("tenants/clean/l.flow.nml: error[NML2083]")
                && stderr.contains("error: 1 skipped path(s) hold content no verb judged"),
            "{verb}: {stderr}"
        );
        let (code, rows) = json_rows(&[verb, "--json", "--root", &root, &target]);
        assert_eq!(code, 1, "{verb}");
        assert_eq!(rows.last().unwrap()["exit"], 1, "{verb}: {rows:?}");
        assert_eq!(rows.last().unwrap()["errors"], 1, "{verb}: {rows:?}");
    }
}

/// The closing row's `skipped` report under the printing budget's
/// discipline: `byWhy` counts EVERYTHING the walk left out, exactly;
/// `rows` are the first `--max-findings` in walk order (depth, then key)
/// and `hidden` says how many the budget held back; `0` lifts it. A
/// tree with thousands of committed links used to grow the closing row
/// by thousands of rows, bounded only by the entry budget.
#[cfg(unix)]
#[test]
fn the_closing_rows_skipped_report_is_bounded_by_the_printing_budget() {
    let dir = scratch_dir("skipped-bounded");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    let cu = dir.join("tenants/cu");
    for i in 0..600 {
        std::os::unix::fs::symlink("plain.flow.nml", cu.join(format!("l{i:03}"))).unwrap();
    }
    std::fs::create_dir_all(cu.join(".hidden")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules")).unwrap();
    let root = dir.display().to_string();
    let plain = cu.join("plain.flow.nml").display().to_string();
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &plain]);
    assert_eq!(code, 0);
    let skipped = &rows.last().unwrap()["skipped"];
    assert_eq!(skipped["byWhy"]["symlink"], 600, "{skipped}");
    assert_eq!(skipped["byWhy"]["dotDirectory"], 1, "{skipped}");
    assert_eq!(skipped["byWhy"]["policyDirectory"], 1, "{skipped}");
    assert_eq!(skipped["shown"], 512, "{skipped}");
    assert_eq!(skipped["hidden"], 90, "{skipped}");
    assert_eq!(skipped["rows"].as_array().unwrap().len(), 512);
    // Walk order: the shallowest first (`node_modules` at the root).
    assert_eq!(skipped["rows"][0]["key"], "node_modules", "{skipped}");
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--max-findings",
        "3",
        "--root",
        &root,
        &plain,
    ]);
    let skipped = &rows.last().unwrap()["skipped"];
    assert_eq!(skipped["shown"], 3, "{skipped}");
    assert_eq!(skipped["hidden"], 599, "{skipped}");
    assert_eq!(skipped["byWhy"]["symlink"], 600, "{skipped}");
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--max-findings",
        "0",
        "--root",
        &root,
        &plain,
    ]);
    let skipped = &rows.last().unwrap()["skipped"];
    assert_eq!(skipped["shown"], 602, "{skipped}");
    assert_eq!(skipped["hidden"], 0, "{skipped}");
}

/// The shadow check cut short by the walk bound REFUSES: a tenant's
/// content 61 directories below a submodule-shaped `.git` FILE fence
/// (its own manifest beside it) used to derive at the tenant's manifest
/// with `shadowed: null` and judge the file under the TENANT's schema —
/// the r80-sec F6 attack surviving through the bound. Exit 2, `pass
/// --root`; under `--root .` the key itself is past the bound.
#[cfg(unix)]
#[test]
fn a_shadow_check_the_walk_bound_cuts_short_refuses_derivation() {
    let dir = scratch_dir("shadow-bound");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    // The fixture's tenant config would be a marker one step above the
    // fence; the shape under test has the operator's marker three
    // directories up, past the bound.
    std::fs::remove_file(dir.join("tenants/cu/nml-project.nml")).unwrap();
    let sub = dir.join("tenants/cu/sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../../../.git/modules/sub\n").unwrap();
    std::fs::write(
        sub.join("evil.package.nml"),
        "package evil:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - mine:\n        files:\n            - \"**/*.flow.nml\"\n        schemas:\n            - core\n",
    )
    .unwrap();
    std::fs::write(
        sub.join("core.model.nml"),
        "model thing:\n    v string\n    extra int\n",
    )
    .unwrap();
    let mut chain = sub.clone();
    for i in 1..=61 {
        chain = chain.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&chain).unwrap();
    std::fs::write(
        chain.join("x.flow.nml"),
        "thing base:\n    v = \"b\"\n    extra = 1\n",
    )
    .unwrap();
    let target = chain.join("x.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", &target]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("the shadow check above the fence at `")
            && stderr.contains("reached the walk bound of 64 directories at `")
            && stderr.contains("(pass --root <dir>)"),
        "{stderr}"
    );
    assert!(
        !stdout.contains("NML2008"),
        "judged under the tenant's schema: {stdout}"
    );
    let (code, rows) = json_rows(&["check", "--json", &target]);
    assert_eq!(code, 2);
    assert_eq!(rows.last().unwrap()["exit"], 2);
    assert!(rows.last().unwrap()["root"].is_null(), "{rows:?}");
    // Three directories shallower the check finishes and refuses on the
    // operator's marker (a FILE fence), as before.
    let mut shallower = sub.clone();
    for i in 1..=58 {
        shallower = shallower.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&shallower).unwrap();
    std::fs::copy(chain.join("x.flow.nml"), shallower.join("x.flow.nml")).unwrap();
    let (code, _, stderr) = run(&["check", &shallower.join("x.flow.nml").display().to_string()]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("sits above the fence at"), "{stderr}");
}

/// The hidden audit's budget is the RUN's — `MAX_TOTAL_ENTRIES`, the
/// universe's own backstop — so a developer's `.venv` past the 65,536
/// unit bound audits whole under `nml check .` (it used to be an error,
/// "could not audit whole"), while a `.nml` found in it is still the
/// gate's error.
#[cfg(unix)]
#[test]
fn a_hidden_directory_past_the_unit_bound_audits_whole_under_the_runs_budget() {
    let dir = scratch_dir("venv-audit");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    let clean = dir.join("tenants/clean");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::copy(
        fixture("workspace").join("tenants/cu/plain.flow.nml"),
        clean.join("plain.flow.nml"),
    )
    .unwrap();
    let venv = clean.join(".venv/lib");
    std::fs::create_dir_all(&venv).unwrap();
    for i in 0..65_600 {
        std::fs::File::create(venv.join(format!("f{i}"))).unwrap();
    }
    let root = dir.display().to_string();
    let tenants = clean.display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &tenants]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stderr.contains("could not audit whole"), "{stderr}");
    std::fs::write(
        venv.join("planted.flow.nml"),
        "thing base:\n    v = \"b\"\n",
    )
    .unwrap();
    let (code, _, stderr) = run(&["check", "--root", &root, &tenants]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "tenants/clean/.venv: error[NML2090]: the walk skipped `tenants/clean/.venv`: a \
             dot-directory it never enters, holding 1 `.nml` file(s) no verb judged \
             (`tenants/clean/.venv/lib/planted.flow.nml`)"
        ),
        "{stderr}"
    );
}

/// r80-sec F6: a `.git` entry a tenant plants below the operator's
/// manifest — a file (`gitdir: …`, the submodule shape git itself
/// writes), a directory, a dangling link, a FIFO — used to re-fence the
/// `--root`-less check at the tenant's directory silently: a file
/// crafted valid under the tenant's own manifest read `ok` while
/// `--root .` said NML2001. Now the shadow check refuses derivation for
/// every kind, naming the marker and the fence, exit 2 (the invocation
/// names the root); `--root .` is unchanged.
#[cfg(unix)]
#[test]
fn a_planted_git_entry_below_the_operators_manifest_refuses_derivation() {
    for kind in ["file", "dir", "link", "fifo"] {
        let dir = scratch_dir(&format!("shadow-{kind}"));
        copy_tree(&fixture("workspace"), &dir);
        let _ = std::fs::remove_dir_all(dir.join("expected"));
        let cu = dir.join("tenants/cu");
        std::fs::write(
            cu.join("evil.package.nml"),
            "package evil:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - mine:\n        files:\n            - \"**/*.flow.nml\"\n        schemas:\n            - core\n",
        )
        .unwrap();
        std::fs::write(cu.join("core.model.nml"), "model thing:\n    v string\n").unwrap();
        match kind {
            "file" => std::fs::write(cu.join(".git"), "gitdir: /nonexistent\n").unwrap(),
            "dir" => std::fs::create_dir_all(cu.join(".git")).unwrap(),
            "link" => std::os::unix::fs::symlink("/nonexistent/nowhere", cu.join(".git")).unwrap(),
            _ => assert!(
                std::process::Command::new("mkfifo")
                    .arg(cu.join(".git"))
                    .status()
                    .unwrap()
                    .success()
            ),
        }
        let (code, stdout, stderr) = run_in(&dir, &["check", "tenants/cu/plain.flow.nml"]);
        let canonical = std::fs::canonicalize(&dir).unwrap();
        if kind == "dir" {
            // A `.git` DIRECTORY below the operator's manifest is a
            // nested checkout — a shape no commit produces (git drops a
            // `.git` path from the index) — so E21's fence holds: the
            // tenant's universe is derived and the marker above it is
            // DISCLOSED on stderr and on the wire, never silently.
            assert_eq!(code, 0, "{kind}: {stdout}{stderr}");
            assert!(stdout.contains(": ok"), "{kind}: {stdout}");
            // r88 (P5): the run stands at `dir`, so the root, the marker
            // and the `--root` advice are spelled from there.
            assert!(
                stderr.contains(
                    "note: workspace root tenants/cu  (derived within the .git fence; SHADOWED \
                     by the root marker `demo.package.nml` above it — pass --root to pin, or \
                     --root . to check under that universe)"
                ),
                "{kind}: {stderr} (canonical {})",
                canonical.display()
            );
            let (_, rows) = json_rows(&[
                "check",
                "--json",
                &canonical
                    .join("tenants/cu/plain.flow.nml")
                    .display()
                    .to_string(),
            ]);
            let root = &rows.last().unwrap()["root"];
            assert_eq!(root["fence"], "dir", "{root}");
            assert_eq!(
                root["shadowed"],
                canonical.join("demo.package.nml").display().to_string(),
                "{root}"
            );
            let (code, stdout, stderr) =
                run_in(&dir, &["check", "--root", ".", "tenants/cu/plain.flow.nml"]);
            assert_eq!(code, 0, "{kind}: {stdout}{stderr}");
            assert!(stdout.contains(": ok"), "{kind}: {stdout}");
            continue;
        }
        assert_eq!(code, 2, "{kind}: {stdout}{stderr}");
        // The refusal spells the target as typed and the marker and the
        // fence from the working directory (the run stands at `dir`),
        // and names the `--root` that checks under the manifest.
        assert!(
            stderr.contains(
                "cannot derive a workspace root for tenants/cu/plain.flow.nml: the root \
                 marker `demo.package.nml` sits above the fence at `tenants/cu/.git`, and \
                 that fence is no directory — a submodule's or linked worktree's .git file, \
                 or a planted entry, below a workspace manifest cannot shrink its universe \
                 (pass --root <dir>; --root . checks under that manifest)"
            ),
            "{kind}: {stderr}"
        );
        assert!(
            !stderr.contains(&canonical.display().to_string()),
            "{kind}: an absolute path where a relative one is right: {stderr}"
        );
        assert!(!stdout.contains(": ok"), "{kind}: never judged: {stdout}");
        // `--root .` is unchanged: the tenant's manifest is inert.
        let (code, stdout, stderr) =
            run_in(&dir, &["check", "--root", ".", "tenants/cu/plain.flow.nml"]);
        assert_eq!(code, 0, "{kind}: {stdout}{stderr}");
        assert!(stdout.contains(": ok"), "{kind}: {stdout}");
        // The `binding` verb refuses the same way: exit 2, the same sentence.
        let (code, _, stderr) = run_in(&dir, &["binding", "tenants/cu/plain.flow.nml"]);
        assert_eq!(code, 2, "{kind}: {stderr}");
        assert!(
            stderr.contains("sits above the fence at"),
            "{kind}: {stderr}"
        );
        // Under `--json` the refusal is the usage-class error row.
        let out = nml_bin()
            .current_dir(&dir)
            .args(["check", "--json", "tenants/cu/plain.flow.nml"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{kind}");
        let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
        let err = rows.iter().find(|r| r["type"] == "error").unwrap();
        assert_eq!(err["kind"], "usage", "{kind}: {err}");
        assert_eq!(rows.last().unwrap()["exit"], 2);
    }
}

/// r80-sec F6: a submodule-shaped fence with NO marker above it — a
/// `.git` FILE below an outer repository — derives at its own manifest
/// and is DISCLOSED: the `--json` root object carries `fence: "file"` and
/// `shadowed: <outer>`, the `binding` line says so, and human mode
/// prints the root once on stderr (a shape the human `check` line used
/// to hide). `--quiet` keeps errors only.
#[cfg(unix)]
#[test]
fn a_shadowed_submodule_fence_is_disclosed_in_every_mode() {
    let dir = scratch_dir("shadowed");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let sub = dir.join("tenants/cu/sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../../../.git/modules/sub\n").unwrap();
    std::fs::copy(
        fixture("workspace").join("demo.package.nml"),
        sub.join("demo.package.nml"),
    )
    .unwrap();
    std::fs::copy(
        fixture("workspace").join("core.model.nml"),
        sub.join("core.model.nml"),
    )
    .unwrap();
    std::fs::create_dir_all(sub.join("tenants/x")).unwrap();
    std::fs::copy(
        fixture("workspace").join("tenants/cu/plain.flow.nml"),
        sub.join("tenants/x/plain.flow.nml"),
    )
    .unwrap();
    let outer = std::fs::canonicalize(&dir).unwrap();
    let target = "tenants/cu/sub/tenants/x/plain.flow.nml";
    let (code, stdout, stderr) = run_in(&dir, &["check", target]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok"), "{stdout}");
    // r88 (P5): the run stands at `dir`, so the root and the shadowing
    // entry are spelled from there.
    let note = "note: workspace root tenants/cu/sub  (derived within a .git FILE fence — a \
                linked worktree's, a submodule's or a planted entry; SHADOWED by another .git \
                entry at `.git` above it — pass --root to pin)"
        .to_string();
    let _ = &outer;
    assert!(stderr.contains(&note), "{stderr}");
    let (code, _, stderr) = run_in(&dir, &["check", "-q", target]);
    assert_eq!(code, 0);
    assert!(!stderr.contains("note: workspace root"), "quiet: {stderr}");
    let out = nml_bin()
        .current_dir(&dir)
        .args(["check", "--json", target])
        .output()
        .unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = rows_of(&String::from_utf8_lossy(&out.stdout));
    let root = &rows.last().unwrap()["root"];
    assert_eq!(root["origin"], "derivedVcsFence", "{root}");
    assert_eq!(root["fence"], "file", "{root}");
    assert_eq!(
        root["shadowed"],
        outer.join(".git").display().to_string(),
        "{root}"
    );
    let (code, stdout, _) = run_in(&dir, &["binding", target]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("SHADOWED by another .git entry at"),
        "{stdout}"
    );
    let (_, rows) = json_rows(&[
        "binding",
        "--json",
        &outer.join(target).display().to_string(),
    ]);
    let row = rows.iter().find(|r| r["type"] == "binding").unwrap();
    assert_eq!(
        row["root"]["shadowed"],
        outer.join(".git").display().to_string(),
        "{row}"
    );
    assert_eq!(row["root"]["fence"], "file", "{row}");
    // An explicit root carries neither: `fence` and `shadowed` are null.
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        outer.to_str().unwrap(),
        &outer.join(target).display().to_string(),
    ]);
    let root = &rows.last().unwrap()["root"];
    assert_eq!(root["origin"], "explicit");
    assert!(
        root["fence"].is_null() && root["shadowed"].is_null(),
        "{root}"
    );
}

/// r80-sec F6(b): a `.git` FILE fence with nothing above it (no outer
/// repository, no marker) is disclosed for its kind alone; a `.git`
/// DIRECTORY fence with nothing above prints no note — the ordinary
/// checkout stays quiet.
#[cfg(unix)]
#[test]
fn a_git_file_fence_is_disclosed_and_a_plain_checkout_is_quiet() {
    for (kind, disclosed) in [("file", true), ("dir", false)] {
        let dir = unfenced_temp_dir(&format!("fence-kind-{kind}"));
        copy_tree(&fixture("workspace"), &dir);
        let _ = std::fs::remove_dir_all(dir.join("expected"));
        match kind {
            "file" => std::fs::write(dir.join(".git"), "gitdir: /nonexistent\n").unwrap(),
            _ => std::fs::create_dir_all(dir.join(".git")).unwrap(),
        }
        let (code, stdout, stderr) = run_in(&dir, &["check", "tenants/cu/plain.flow.nml"]);
        assert_eq!(code, 0, "{kind}: {stdout}{stderr}");
        assert_eq!(
            stderr.contains("note: workspace root"),
            disclosed,
            "{kind}: {stderr}"
        );
        if disclosed {
            assert!(
                stderr.contains("within a .git FILE fence — a linked worktree's, a submodule's or a planted entry — pass --root to pin"),
                "{stderr}"
            );
            assert!(!stderr.contains("SHADOWED"), "{stderr}");
        }
        let (_, rows) = json_rows(&[
            "check",
            "--json",
            &dir.join("tenants/cu/plain.flow.nml").display().to_string(),
        ]);
        let root = &rows.last().unwrap()["root"];
        assert_eq!(root["fence"], kind, "{root}");
        assert!(root["shadowed"].is_null(), "{root}");
    }
}

/// r80-sec F9: a target more than 64 directories below the nearest
/// `.git` derived a component-cap root — an OPEN universe at the
/// target's own directory in which a file 70 directories below the
/// operator's manifest was `ok` (`--root .` said NML2001). Now the walk
/// bound derives nothing: closed-denied, exit 2, "pass --root"; and
/// `derivedComponentCap` is no longer a `root.origin` value.
#[cfg(unix)]
#[test]
fn a_target_past_the_walk_bound_with_no_fence_refuses_derivation() {
    let dir = unfenced_temp_dir("deep-chain");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let mut chain = dir.join("tenants/cu");
    for i in 1..=68 {
        chain = chain.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&chain).unwrap();
    std::fs::write(
        chain.join("deep.flow.nml"),
        "thing base:\n    v = \"b\"\n    extra = 1\n",
    )
    .unwrap();
    let target = chain.join("deep.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["check", &target]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("no VCS root within 64 directories above `"),
        "{stderr}"
    );
    assert!(stderr.contains("(pass --root <dir>)"), "{stderr}");
    assert!(!stdout.contains(": ok"), "{stdout}");
    let (code, rows) = json_rows(&["check", "--json", &target]);
    assert_eq!(code, 2);
    assert_eq!(rows.last().unwrap()["exit"], 2);
    assert!(
        !rows
            .iter()
            .any(|r| r.to_string().contains("derivedComponentCap")),
        "{rows:?}"
    );
    // The key itself is past the bound under `--root .`: refused as before.
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", &target]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("more than 64 path components"), "{stderr}");
}

/// r80-sec F3: the workspace-free verbs read through the target cap.
/// `parse`/`fmt` read whole — one 256 MiB zero-file (sparse on disk,
/// ~250 KB in git) reached 20–22 GB resident before the first diagnostic
/// — and now refuse at 16 MiB in the reader's words, exit 1, the closing
/// row intact.
#[test]
fn parse_and_fmt_refuse_a_target_past_the_byte_cap() {
    let dir = scratch_dir("free-cap");
    let big = dir.join("big.flow.nml");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    for verb in ["parse", "fmt"] {
        let (code, stdout, stderr) = run(&[verb, big.to_str().unwrap()]);
        assert_eq!(code, 1, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.contains("too large: over 16 MiB (16777217 bytes) — a file is read only up to 16 MiB (16777216 bytes)"),
            "{verb}: {stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
        let (code, rows) = json_rows(&[verb, "--json", big.to_str().unwrap()]);
        assert_eq!(code, 1, "{verb}");
        assert_eq!(rows.last().unwrap()["exit"], 1, "{verb}: {rows:?}");
    }
    // Exactly the cap reads.
    std::fs::File::create(&big)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let (_, _, stderr) = run(&["parse", big.to_str().unwrap()]);
    assert!(!stderr.contains("too large"), "{stderr}");
}

/// NML2088's manifest-validation form is LOCATED at the first finding —
/// a parse error, a meta-schema finding — `key:line:col:` in the human
/// line and `line`/`col` on the `--json` wire, as every located finding
/// prints; the sentence names no line (the location is the row's own,
/// once). It used to fold `at <key>:<line>:<col>` into the sentence
/// while the wire row read `line: null` and the editor placed it at
/// 1:1 (r94 cov F1); before that it ended in a count (`(1 error(s))`).
#[test]
fn an_unloadable_manifest_row_is_located_at_its_first_finding() {
    let dir = workspace_copy("manifest-location");
    std::fs::write(
        dir.join("tenants/cu/tenant.package.nml"),
        "package tenant:\n    version = \"0.1.0\"\n  formatVersion = 1\n",
    )
    .unwrap();
    let root = dir.join("tenants/cu");
    let target = dir.join("tenants/cu/plain.flow.nml");
    let (code, _, stderr) = run(&[
        "check",
        "--root",
        root.to_str().unwrap(),
        target.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "tenant.package.nml:3:1: error[NML2088]: manifest failed to load: indentation of 2 \
             matches no enclosing block"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("error(s))"), "{stderr}");
    assert!(!stderr.contains("validation at "), "{stderr}");
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        root.to_str().unwrap(),
        target.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["source"], "tenant.package.nml", "{row}");
    assert_eq!(row["line"], 3, "{row}");
    assert_eq!(row["col"], 1, "{row}");
    // A parse failure locates the same way: the `layers:` block a
    // remedy printed, pasted at the ITEM column of `[]validator` (the
    // shape that used to lower to nothing and load clean).
    let dir = workspace_copy("manifest-location-parse");
    let manifest = std::fs::read_to_string(dir.join("demo.package.nml")).unwrap();
    let pasted = manifest.replace(
        "        strict = true\n",
        "        strict = true\n    layers:\n        allowRefs:\n            - \"tenants/**\"\n",
    );
    assert_ne!(pasted, manifest);
    std::fs::write(dir.join("demo.package.nml"), pasted).unwrap();
    let file = "tenants/cu/member-lookup.flow.nml";
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "demo.package.nml:16:5: error[NML2088]: manifest failed to load: expected a list \
             item, a property, a modifier or a shared property in an array body, found a \
             nested block\n"
        ),
        "{stderr}"
    );
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        &dir.display().to_string(),
        &dir.join(file).display().to_string(),
    ]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["line"], 16, "{row}");
    assert_eq!(row["col"], 5, "{row}");
    // `binding` states the universe's word once, located the same way.
    let (code, _, stderr) = run_in(&dir, &["binding", "--root", ".", file]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("demo.package.nml:16:5: error[NML2088]: manifest failed to load: "),
        "{stderr}"
    );
}

/// r85 D8: a target INSIDE an unreadable directory of a denied unit gets
/// the unit's own NML2089 row — exit 1 in `check`, `validate`, `fix` AND
/// `binding` — answered before any probe under the unit. It used to reach
/// the OS's EACCES on its own path as a bare `error: <path>: permission
/// denied on a path component` (exit 1 in `check`, 2 in `binding`) with
/// the unit's row unspoken.
#[cfg(unix)]
#[test]
fn a_target_inside_an_unreadable_unit_gets_the_units_row_in_every_verb() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("unit-eacces-target");
    std::fs::create_dir_all(dir.join("tenants/cu/locked/sub")).unwrap();
    std::fs::write(
        dir.join("tenants/cu/locked/sub/l.flow.nml"),
        "thing l:\n    v = \"l\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.join("tenants/cu/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(dir.join("tenants/cu/locked"));
    if std::fs::read_dir(dir.join("tenants/cu/locked")).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.to_str().unwrap();
    let target = dir.join("tenants/cu/locked/sub/l.flow.nml");
    let denial = "tenants/cu/locked/sub/l.flow.nml: error[NML2089]: discovery under `tenants/cu` \
                  was cut short: the walk stopped at `tenants/cu/locked` (unreadable: permission \
                  denied on a path component)";
    for verb in [
        vec!["check"],
        vec!["validate"],
        vec!["fix", "--dry-run"],
        vec!["binding"],
    ] {
        let mut args = verb.clone();
        args.extend(["--root", root, target.to_str().unwrap()]);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 1, "{verb:?}: {stdout}{stderr}");
        // The checking verbs print the row on stderr; `binding` on its
        // `notes` line (stdout).
        assert!(
            format!("{stdout}{stderr}").contains(denial),
            "{verb:?}: {stdout}{stderr}"
        );
        assert!(
            !stderr
                .lines()
                .any(|l| l.starts_with("error: ") && l.contains("permission denied")),
            "the OS error, bare: {verb:?}: {stderr}"
        );
    }
    let (code, rows) = json_rows(&[
        "binding",
        "--json",
        "--root",
        root,
        target.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert_eq!(rows[0]["type"], "binding", "{}", rows[0]);
    assert_eq!(rows[0]["governing"], "unbound", "{}", rows[0]);
    assert_eq!(rows[0]["notes"][0]["code"], "NML2089", "{}", rows[0]);
    assert!(
        !rows.iter().any(|r| r["kind"] == "usage"),
        "never the invocation's mistake: {rows:?}"
    );
}

/// r85 D9: `binding` on an absent file says `(absent)` on the `file`
/// line — under `-q` too, where the warning is silent — and `absent:
/// true` on the row; an existing file says neither. The exit is
/// unchanged: the question asked is what WOULD govern the path.
#[test]
fn binding_says_absent_on_the_file_line_and_the_row() {
    let dir = workspace_copy("binding-absent-line");
    let root = dir.to_str().unwrap();
    let nope = dir.join("tenants/cu/nope.flow.nml").display().to_string();
    for args in [
        vec!["binding", "--root", root, &nope],
        vec!["binding", "-q", "--root", root, &nope],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(
            stdout.starts_with("file      tenants/cu/nope.flow.nml  (absent)\n"),
            "{args:?}: {stdout}"
        );
    }
    let (code, rows) = json_rows(&["binding", "--json", "-q", "--root", root, &nope]);
    assert_eq!(code, 0);
    assert_eq!(rows[0]["type"], "binding", "{}", rows[0]);
    assert_eq!(rows[0]["absent"], true, "{}", rows[0]);
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, stdout, stderr) = run(&["binding", "--root", root, &plain]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("file      tenants/cu/plain.flow.nml\n"),
        "{stdout}"
    );
    let (_, rows) = json_rows(&["binding", "--json", "--root", root, &plain]);
    assert_eq!(rows[0]["absent"], false, "{}", rows[0]);
}

/// r85 D10: `nml explain A B …` prints one document per code (a blank
/// line between two), one `explain` row each under `--json`; an unknown
/// code among many is its own `error` row (kind `target`), the run goes
/// on, and exits 1.
#[test]
fn explain_takes_many_codes() {
    let (code, stdout, stderr) = run(&["explain", "NML2080", "2083"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("# NML2080\n"), "{stdout}");
    assert!(stdout.contains("\n\n# NML2083\n"), "{stdout}");
    let (code, rows) = json_rows(&["explain", "--json", "NML2080", "NML2083"]);
    assert_eq!(code, 0);
    let codes: Vec<&str> = rows
        .iter()
        .filter(|r| r["type"] == "explain")
        .map(|r| r["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["NML2080", "NML2083"], "{rows:?}");
    assert_eq!(rows.last().unwrap()["type"], "summary");
    let (code, stdout, stderr) = run(&["explain", "NML2080", "NML9999"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.starts_with("# NML2080\n"), "{stdout}");
    assert!(
        stderr.contains("error: no such diagnostic code: NML9999"),
        "{stderr}"
    );
    assert!(stderr.contains("error: 1 of 2 code(s) unknown"), "{stderr}");
    let (code, rows) = json_rows(&["explain", "--json", "NML9999", "NML2080"]);
    assert_eq!(code, 1);
    assert_eq!(rows[0]["type"], "error", "{}", rows[0]);
    assert_eq!(rows[0]["kind"], "target", "{}", rows[0]);
    assert_eq!(rows[1]["type"], "explain", "{}", rows[1]);
    assert_eq!(rows.last().unwrap()["exit"], 1, "{rows:?}");
}

/// r85 (r84-sec F1): the gate's row for a hidden directory is ONE per
/// directory — the directory, the exact count, up to eight example keys
/// — never one per file: 2,001 committed `.nml` files under `.hidden`
/// are one error row, `errors: 1` on the closing row; a directory of
/// three names all three and no tail.
#[cfg(unix)]
#[test]
fn the_gate_reports_a_hidden_directory_once_with_its_count() {
    let dir = workspace_copy("gate-hidden-count");
    let root = dir.to_str().unwrap().to_string();
    // A fresh tenant with one clean file: the closing row's `errors` is
    // then the gate's alone.
    let many = dir.join("tenants/many");
    std::fs::create_dir_all(&many).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        many.join("plain.flow.nml"),
    )
    .unwrap();
    let hidden = many.join(".hidden");
    std::fs::create_dir_all(hidden.join("sub")).unwrap();
    for i in 0..2000 {
        std::fs::File::create(hidden.join(format!("f{i:04}.flow.nml"))).unwrap();
    }
    std::fs::File::create(hidden.join("sub/deep.flow.nml")).unwrap();
    let cu = many.display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &cu]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(stderr.matches("error[NML2090]").count(), 1, "{stderr}");
    assert!(
        stderr.contains(
            "tenants/many/.hidden: error[NML2090]: the walk skipped `tenants/many/.hidden`: a \
             dot-directory it never enters, holding 2001 `.nml` file(s) no verb judged \
             (`tenants/many/.hidden/f0000.flow.nml`, `tenants/many/.hidden/f0001.flow.nml`, \
             `tenants/many/.hidden/f0002.flow.nml`, `tenants/many/.hidden/f0003.flow.nml`, \
             `tenants/many/.hidden/f0004.flow.nml`, `tenants/many/.hidden/f0005.flow.nml`, \
             `tenants/many/.hidden/f0006.flow.nml`, `tenants/many/.hidden/f0007.flow.nml`, and \
             1993 more) — content a runtime could read; move it where the walk lists it, or name \
             the files on the command line"
        ),
        "{stderr}"
    );
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &cu]);
    assert_eq!(code, 1);
    assert_eq!(
        rows.iter().filter(|r| r["code"] == "NML2090").count(),
        1,
        "{rows:?}"
    );
    let last = rows.last().unwrap();
    assert_eq!(last["errors"], 1, "{last}");
    assert_eq!(last["skipped"]["byWhy"]["dotDirectory"], 1, "{last}");
    // Three: all named, no tail.
    let few = dir.join("tenants/du");
    std::fs::create_dir_all(few.join(".few")).unwrap();
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        few.join("plain.flow.nml"),
    )
    .unwrap();
    for n in ["a", "b", "c"] {
        std::fs::File::create(few.join(format!(".few/{n}.flow.nml"))).unwrap();
    }
    let du = few.display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &du]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "holding 3 `.nml` file(s) no verb judged (`tenants/du/.few/a.flow.nml`, \
             `tenants/du/.few/b.flow.nml`, `tenants/du/.few/c.flow.nml`) — content"
        ),
        "{stderr}"
    );
}

/// The maximum resident set size of one `nml` run, in bytes, measured
/// by `/usr/bin/time` (`-l` on macOS, `-v` on Linux) alongside its wall
/// time; `None` for the RSS where the tool is absent.
#[cfg(unix)]
fn timed_with_rss(args: &[&str]) -> (std::time::Duration, Option<u64>, i32) {
    let flag = if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    };
    let start = std::time::Instant::now();
    let out = std::process::Command::new("/usr/bin/time")
        .arg(flag)
        .arg(nml_bin().get_program())
        .args(args)
        .output();
    let elapsed = start.elapsed();
    let Ok(out) = out else {
        let out = nml_bin().args(args).output().expect("failed to run nml");
        return (start.elapsed(), None, out.status.code().unwrap_or(-1));
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let rss = stderr.lines().find_map(|l| {
        let l = l.trim();
        if let Some(n) = l.strip_suffix("maximum resident set size") {
            return n.trim().parse::<u64>().ok();
        }
        if let Some(n) = l.strip_prefix("Maximum resident set size (kbytes):") {
            return n.trim().parse::<u64>().ok().map(|k| k * 1024);
        }
        None
    });
    (elapsed, rss, out.status.code().unwrap_or(-1))
}

/// r85 (r84-sec F1): the gate over a tenant's committed dot-directory
/// of 300,000 `.nml` files — one row, an exact count — is linear and
/// bounded in memory: within 3 s and 80 MB on the release perf tier
/// (it took 89.5 s and 259 MB with a row per file and a linear dedup;
/// ~18 minutes and ~1 GB at the audit bound).
#[cfg(unix)]
#[test]
#[ignore = "perf tier: run with `cargo test -p nml-cli --release --test cli_tests -- --ignored perf_`"]
fn perf_the_gate_over_a_hidden_flood_is_linear_and_bounded_in_memory() {
    let dir = workspace_copy("gate-hidden-flood");
    let root = dir.to_str().unwrap().to_string();
    let hidden = dir.join(".hidden");
    std::fs::create_dir_all(&hidden).unwrap();
    for i in 0..300_000 {
        std::fs::File::create(hidden.join(format!("f{i}.flow.nml"))).unwrap();
    }
    let (elapsed, rss, code) = timed_with_rss(&["check", "--root", &root, &root]);
    assert_eq!(code, 1);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the gate over 300,000 hidden files took {elapsed:?}"
    );
    match rss {
        Some(bytes) => assert!(
            bytes <= 80 * 1024 * 1024,
            "the gate over 300,000 hidden files held {} MB",
            bytes / (1024 * 1024)
        ),
        None => eprintln!("/usr/bin/time unavailable: the RSS half of this pin did not run"),
    }
    let (code, rows) = json_rows(&["check", "--json", "--root", &root, &root]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2090")
        .expect("the directory's row");
    assert!(
        row["message"]
            .as_str()
            .unwrap()
            .contains("holding 300000 `.nml` file(s) no verb judged"),
        "{row}"
    );
    // ONE row for the directory (the fixture's own findings ride the
    // closing row's `errors` beside it).
    assert_eq!(
        rows.iter().filter(|r| r["code"] == "NML2090").count(),
        1,
        "{rows:?}"
    );
}

/// r84-cov (mutant D23 survived): a hidden directory the audit could not
/// finish — unlistable here — is the gate's ERROR row (NML2090 naming
/// the directory, `diag::audit_incomplete`), exit 1; listable again, the
/// same target is clean.
#[cfg(unix)]
#[test]
fn an_unlistable_hidden_directory_is_the_gates_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("gate-hidden-locked");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    let clean = dir.join("tenants/clean");
    std::fs::create_dir_all(clean.join(".hidden/locked")).unwrap();
    std::fs::copy(
        fixture("workspace").join("tenants/cu/plain.flow.nml"),
        clean.join("plain.flow.nml"),
    )
    .unwrap();
    std::fs::set_permissions(
        clean.join(".hidden/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(clean.join(".hidden/locked"));
    if std::fs::read_dir(clean.join(".hidden/locked")).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.display().to_string();
    let target = clean.display().to_string();
    let (code, stdout, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "tenants/clean/.hidden/locked: error[NML2090]: the walk skipped \
             `tenants/clean/.hidden/locked`: a hidden directory the gate could not audit whole"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("error: 1 skipped path(s) hold content no verb judged"),
        "{stderr}"
    );
    std::fs::set_permissions(
        clean.join(".hidden/locked"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (code, _, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 0, "listable again: {stderr}");
    assert!(!stderr.contains("NML2090"), "{stderr}");
}

/// r84-cov (mutant N14 survived): a file argument typed twice is taken
/// once — one `result` row, `targets` counts it once, `fix` says "of 1
/// file(s)" — as a directory reached twice is.
#[test]
fn a_file_argument_typed_twice_runs_once() {
    let dir = workspace_copy("file-twice");
    let root = dir.to_str().unwrap();
    let plain = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let (code, rows) = json_rows(&["check", "--json", "--root", root, &plain, &plain]);
    assert_eq!(code, 0);
    assert_eq!(
        rows.iter().filter(|r| r["type"] == "result").count(),
        1,
        "{rows:#?}"
    );
    assert_eq!(rows.last().unwrap()["targets"], 1, "{rows:#?}");
    let (code, stdout, stderr) = run(&["fix", "--dry-run", "--root", root, &plain, &plain]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("of 1 file(s)"), "{stdout}");
}

/// r85 (r84-cov P1): `version` is a verb like every other — `nml version
/// --help` is a page (USAGE, EXIT CODES, EXAMPLES; `nml help version` the
/// same), `nml version --json` is one `version` row then the closing row
/// whose `verb` is `version` for every spelling (`--version`, `-V`), and
/// a surplus argument is refused (it printed the version for any argument
/// and had no page; `--json` was ignored).
#[test]
fn version_is_a_verb_with_a_page_and_a_row() {
    let expected = format!("nml {}\n", env!("CARGO_PKG_VERSION"));
    for spelling in ["version", "--version", "-V"] {
        let (code, stdout, stderr) = run(&[spelling]);
        assert_eq!(code, 0, "{spelling}: {stderr}");
        assert_eq!(stdout, expected, "{spelling}");
        let (code, rows) = json_rows(&[spelling, "--json"]);
        assert_eq!(code, 0, "{spelling}");
        assert_eq!(rows[0]["type"], "version", "{spelling}: {}", rows[0]);
        assert_eq!(rows[0]["version"], env!("CARGO_PKG_VERSION"), "{}", rows[0]);
        let last = rows.last().unwrap();
        assert_eq!(last["type"], "summary", "{last}");
        assert_eq!(last["verb"], "version", "{spelling}: {last}");
        assert_eq!(last["formatVersion"], 1, "{last}");
        assert_eq!(rows.len(), 2, "{rows:?}");
    }
    for args in [
        vec!["version", "--help"],
        vec!["version", "-h"],
        vec!["help", "version"],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(
            stdout.starts_with("usage: nml version [--json] [--quiet]")
                && stdout.contains("EXIT CODES:")
                && stdout.contains("EXAMPLES:"),
            "{args:?}: {stdout}"
        );
        assert!(stderr.is_empty(), "{args:?}: {stderr}");
    }
}

/// r85 (r84-cov F15): an empty argument is a usage error, exit 2, in
/// every verb — the same class everywhere (`check ""` used to be a file
/// candidate that failed at the read, exit 1, while `binding ""` exited
/// 2), said by name.
#[test]
fn an_empty_argument_is_a_usage_error_in_every_verb() {
    for verb in ["check", "validate", "binding", "parse", "fmt", "explain"] {
        let (code, stdout, stderr) = run(&[verb, ""]);
        assert_eq!(code, 2, "{verb}: {stdout}{stderr}");
        assert!(
            stderr.starts_with(&format!(
                "error: an empty argument is not a path; usage: nml {verb} "
            )),
            "{verb}: {stderr}"
        );
        assert!(stdout.is_empty(), "{verb}: {stdout}");
    }
    let (code, rows) = json_rows(&["check", "--json", ""]);
    assert_eq!(code, 2);
    assert_eq!(rows[0]["kind"], "usage", "{}", rows[0]);
}

/// r86: an unlistable subdirectory inside a hidden tree does not hide the
/// rest of it — the siblings are still audited, the directory's row says
/// `at least`, and the unlistable directory is its own error row.
#[cfg(unix)]
#[test]
fn an_unlistable_subdirectory_leaves_the_hidden_count_a_lower_bound() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("gate-hidden-partial");
    copy_tree(&fixture("workspace"), &dir);
    let _ = std::fs::remove_dir_all(dir.join("expected"));
    let clean = dir.join("tenants/clean");
    for d in [".inc/a", ".inc/locked", ".inc/z"] {
        std::fs::create_dir_all(clean.join(d)).unwrap();
    }
    std::fs::copy(
        fixture("workspace").join("tenants/cu/plain.flow.nml"),
        clean.join("plain.flow.nml"),
    )
    .unwrap();
    for f in [
        ".inc/a/1.flow.nml",
        ".inc/locked/3.flow.nml",
        ".inc/z/2.flow.nml",
    ] {
        std::fs::File::create(clean.join(f)).unwrap();
    }
    std::fs::set_permissions(
        clean.join(".inc/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let _unlock = Unlock(clean.join(".inc/locked"));
    if std::fs::read_dir(clean.join(".inc/locked")).is_ok() {
        return; // root: the lock does not bite
    }
    let root = dir.display().to_string();
    let target = clean.display().to_string();
    let (code, _, stderr) = run(&["check", "--root", &root, &target]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "tenants/clean/.inc: error[NML2090]: the walk skipped `tenants/clean/.inc`: a \
             dot-directory it never enters, holding at least 2 `.nml` file(s) no verb judged \
             (`tenants/clean/.inc/a/1.flow.nml`, `tenants/clean/.inc/z/2.flow.nml`)"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "tenants/clean/.inc/locked: error[NML2090]: the walk skipped \
             `tenants/clean/.inc/locked`: a hidden directory the gate could not audit whole"
        ),
        "{stderr}"
    );
}

/// A `.git` entry that is a SYMLINK or a special entry (a FIFO) is a
/// fence like any other — the kernel's derivation stops at it — and is
/// disclosed for its kind, as the FILE fence is: the `note:` line names
/// the planted entry, and the wire's `root.fence` spells `symlink` /
/// `other`. (The FILE and DIRECTORY kinds were pinned; these two arms of
/// `fence_facts` had no pin at the front end.)
#[cfg(unix)]
#[test]
fn a_git_symlink_or_special_entry_fence_is_disclosed_with_its_kind() {
    for (kind, sentence) in [
        (
            "symlink",
            "within a .git SYMLINK fence — a planted entry — pass --root to pin",
        ),
        (
            "other",
            "within a .git special-entry fence — a planted entry — pass --root to pin",
        ),
    ] {
        let dir = unfenced_temp_dir(&format!("fence-kind-{kind}"));
        copy_tree(&fixture("workspace"), &dir);
        let _ = std::fs::remove_dir_all(dir.join("expected"));
        match kind {
            "symlink" => {
                std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
                std::os::unix::fs::symlink("elsewhere", dir.join(".git")).unwrap();
            }
            _ => {
                let status = std::process::Command::new("mkfifo")
                    .arg(dir.join(".git"))
                    .status()
                    .expect("mkfifo runs");
                assert!(status.success(), "mkfifo");
            }
        }
        let (code, stdout, stderr) = run_in(&dir, &["check", "tenants/cu/plain.flow.nml"]);
        assert_eq!(code, 0, "{kind}: {stdout}{stderr}");
        assert!(
            stderr.contains(&format!("note: workspace root .  (derived {sentence})\n")),
            "{kind}: {stderr}"
        );
        assert!(!stderr.contains("SHADOWED"), "{stderr}");
        let (code, stdout, _) = run_in(&dir, &["binding", "tenants/cu/plain.flow.nml"]);
        assert_eq!(code, 0, "{kind}: {stdout}");
        assert!(
            stdout.contains(&format!("root      .  (derived {sentence})\n")),
            "{kind}: {stdout}"
        );
        let (_, rows) = json_rows(&[
            "check",
            "--json",
            &dir.join("tenants/cu/plain.flow.nml").display().to_string(),
        ]);
        let root = &rows.last().unwrap()["root"];
        assert_eq!(root["fence"], kind, "{root}");
        assert_eq!(root["origin"], "derivedVcsFence", "{root}");
        assert!(root["shadowed"].is_null(), "{root}");
    }
}

/// A failing target among many is named ONCE, on its own line: a
/// per-target error that already spells its target (`<target>: no such
/// file or directory`; the reader's `failed to read <target>: …`) is not
/// prefixed a second time by the multi-target driver — the run continues,
/// the others report, and the closing line counts the failure. The
/// `--json` row carries the same message, `kind: target`.
#[cfg(unix)]
#[test]
fn a_failing_target_among_many_is_named_once_on_its_own_line() {
    use std::os::unix::fs::PermissionsExt;
    let dir = workspace_copy("failing-target-named-once");
    std::fs::copy(
        dir.join("tenants/cu/plain.flow.nml"),
        dir.join("tenants/cu/plain2.flow.nml"),
    )
    .unwrap();
    let (plain, nope, plain2) = (
        "tenants/cu/plain.flow.nml",
        "tenants/cu/nope.flow.nml",
        "tenants/cu/plain2.flow.nml",
    );
    let (code, stdout, stderr) = run_in(&dir, &["check", "--root", ".", plain, nope, plain2]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(stdout.matches(": ok (").count(), 2, "{stdout}");
    assert_eq!(
        stderr.matches("no such file or directory").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("error: {nope}: no such file or directory\n")),
        "{stderr}"
    );
    assert!(
        !stderr.contains(&format!("{nope}: {nope}")),
        "the target is named once: {stderr}"
    );
    assert!(
        stderr.contains("error: 1 of 3 file(s) failed\n"),
        "{stderr}"
    );
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        &dir.display().to_string(),
        &dir.join(plain).display().to_string(),
        &dir.join(nope).display().to_string(),
        &dir.join(plain2).display().to_string(),
    ]);
    assert_eq!(code, 1);
    let errors: Vec<&serde_json::Value> = rows.iter().filter(|r| r["type"] == "error").collect();
    // One `target` row; the run's verdict rides the `summary` (no `run` row).
    assert_eq!(errors.len(), 1, "one target row: {rows:?}");
    assert_eq!(errors[0]["kind"], "target", "{}", errors[0]);
    assert_eq!(
        errors[0]["message"],
        format!("{}: no such file or directory", dir.join(nope).display()),
        "{}",
        errors[0]
    );
    let summary = rows.last().unwrap();
    assert_eq!(
        (summary["exit"].as_i64(), summary["targets"].as_u64()),
        (Some(1), Some(3))
    );
    // The reader's own prefix: `failed to read <target>: …`, once.
    std::fs::set_permissions(dir.join(plain2), std::fs::Permissions::from_mode(0o000)).unwrap();
    let _unlock = Unlock(dir.join(plain2));
    if std::fs::read(dir.join(plain2)).is_ok() {
        return; // root: the lock does not bite
    }
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", plain, plain2]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(stderr.matches("failed to read").count(), 1, "{stderr}");
    assert!(
        stderr.contains(&format!("error: failed to read {plain2}: ")),
        "{stderr}"
    );
    assert!(
        !stderr.contains(&format!("{plain2}: failed to read")),
        "the target is named once: {stderr}"
    );
    assert!(
        stderr.contains("error: 1 of 2 file(s) failed\n"),
        "{stderr}"
    );
}

/// A directory the universe walk did not enter — reached through a link
/// an OPEN universe follows (`link/subdir`, the link a developer's own)
/// — is refused by every per-file pipeline in ONE sentence, never read
/// as a file: `check`, `validate` and `fix` say so and exit 1 (`fix`
/// used to read it and print the OS's `Is a directory`), `binding` says
/// it takes files and exits 2.
#[cfg(unix)]
#[test]
fn a_directory_behind_a_link_in_an_open_universe_is_refused_in_the_walks_words() {
    let dir = scratch_dir("dir-behind-a-link");
    std::fs::create_dir_all(dir.join("real/subdir")).unwrap();
    std::fs::write(dir.join("real/subdir/z.nml"), "thing t:\n    v = 1\n").unwrap();
    std::os::unix::fs::symlink("real", dir.join("link")).unwrap();
    let sentence = "error: `link/subdir` is a directory the universe walk did not enter (behind a \
                    link, or inside a denied unit) — name the files, or pass --root to a tree the \
                    walk can list\n";
    for args in [
        &["check", "--root", ".", "link/subdir"][..],
        &["validate", "--root", ".", "link/subdir"],
        &["fix", "--dry-run", "--root", ".", "link/subdir"],
        &["fix", "--root", ".", "link/subdir"],
    ] {
        let (code, stdout, stderr) = run_in(&dir, args);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains(sentence), "{args:?}: {stderr}");
        assert!(!stderr.contains("Is a directory"), "{args:?}: {stderr}");
        if args[0] == "fix" {
            assert!(
                stderr.contains("error: 1 path(s) could not be fixed\n"),
                "{args:?}: {stderr}"
            );
        }
    }
    // The link ITSELF typed as the target (`link`, not `link/subdir`):
    // the resolved leaf is a directory — the same sentence from the
    // kind check at the open, never the OS's `Is a directory (os error
    // 21)`.
    let typed = "error: `link` is a directory the universe walk did not enter (behind a link, \
                 or inside a denied unit) — name the files, or pass --root to a tree the walk \
                 can list\n";
    for args in [
        &["check", "--root", ".", "link"][..],
        &["validate", "--root", ".", "link"],
        &["fix", "--dry-run", "--root", ".", "link"],
    ] {
        let (code, stdout, stderr) = run_in(&dir, args);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains(typed), "{args:?}: {stderr}");
        assert!(!stderr.contains("os error"), "{args:?}: {stderr}");
    }
    // `binding`: the directory behind the link AND the typed link itself
    // are its directory refusal (exit 2) — never a `binding none` block
    // with a symlink note for a path that is not a file.
    for target in ["link/subdir", "link"] {
        let (code, stdout, stderr) = run_in(&dir, &["binding", "--root", ".", target]);
        assert_eq!(code, 2, "{target}: {stdout}{stderr}");
        assert!(
            stderr.contains(&format!(
                "error: `{target}` is a directory — nml binding takes files; name a file under it\n"
            )),
            "{target}: {stderr}"
        );
        assert!(!stdout.contains("binding   none"), "{target}: {stdout}");
    }
    // The content stays reachable by its own name.
    let (code, stdout, stderr) = run_in(&dir, &["check", "--root", ".", "real/subdir/z.nml"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// The fixer's rewrite restores the permission bits the UMASK strips:
/// the temp is created at the original's bits under the umask (a
/// `0664` original under `umask 022` is created `0644`) and the
/// `fchmod` after the write applies the bits exactly — so the file
/// comes back `0664`. Run under an explicit `umask 022` (a child shell),
/// so the pin holds whatever umask the developer's or CI's shell
/// carries. (Every mode pin used `0600`, which no umask strips, so the
/// `fchmod` had no pin — the r92 mutant that dropped it survived.)
#[cfg(unix)]
#[test]
fn fix_restores_the_permission_bits_the_umask_strips() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("fix-umask-bits");
    let file = dir.join("shared.nml");
    std::fs::write(
        &file,
        "model job:\n    timeout duration\n\njob A:\n    timeout = \"30s\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o664,
        "the scratch filesystem keeps group/other bits"
    );
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "umask 022 && exec {} fix --root . shared.nml",
            env!("CARGO_BIN_EXE_nml")
        ))
        .current_dir(&dir)
        .env("NML_UNICODE", "1")
        .output()
        .expect("run nml under umask 022");
    assert!(out.status.success(), "{out:?}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("timeout = 30s"),
        "the fix applied"
    );
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o664, "the bits the umask stripped are restored");
}

/// The gate's rows (NML2090, locationless) obey the finding budget like
/// every located finding — `--max-findings 2` over three dot-files
/// prints at most two and the trailer names what was withheld; the
/// counts on the closing row stay exact. (The locationless reporter's
/// `admit` had no pin: the budget pins ran over located findings.)
#[test]
fn the_gates_locationless_rows_obey_the_finding_budget() {
    let dir = workspace_copy("gate-rows-budget");
    for name in [".a.flow.nml", ".b.flow.nml", ".c.flow.nml"] {
        std::fs::write(
            dir.join("tenants/cu").join(name),
            "thing t:\n    v = \"x\"\n",
        )
        .unwrap();
    }
    let (code, _, stderr) = run_in(
        &dir,
        &["check", "--root", ".", "--max-findings", "2", "tenants/cu"],
    );
    assert_eq!(code, 1, "{stderr}");
    let printed = stderr.matches("error[NML2090]").count();
    assert!(
        (1..=2).contains(&printed),
        "{printed} rows printed under a budget of 2: {stderr}"
    );
    assert!(
        stderr.contains("more finding(s) not shown (limit 2;"),
        "{stderr}"
    );
    let (_, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        &dir.display().to_string(),
        "--max-findings",
        "2",
        &dir.join("tenants/cu").display().to_string(),
    ]);
    let gate_rows = rows.iter().filter(|r| r["code"] == "NML2090").count();
    assert!(gate_rows <= 2, "{rows:?}");
    let summary = rows.last().unwrap();
    assert!(
        summary["withheld"]["hidden"].as_u64().unwrap_or(0) >= 1,
        "{summary}"
    );
    assert!(
        summary["errors"].as_u64().unwrap() >= 3,
        "the counts stay exact: {summary}"
    );
}

/// The unit-layout lint (NML2092) rides a universe that STANDS: under a
/// live manifest that failed to load (NML2088) the run refuses before
/// any target and prints no layout note — a lint about a universe that
/// validates nothing is noise. (The door's order had no pin.)
#[test]
fn the_layout_lint_is_silent_under_a_broken_universe() {
    let dir = scratch_dir("lint-under-broken-universe");
    copy_tree(&fixture("workspace-gap"), &dir);
    std::fs::write(
        dir.join("other.package.nml"),
        "package other:\n    version = \"0.1.0\"\n",
    )
    .unwrap();
    for args in [
        &["check", "--root", ".", "tenants/cu/flows/plain.flow.nml"][..],
        &[
            "fix",
            "--dry-run",
            "--root",
            ".",
            "tenants/cu/flows/plain.flow.nml",
        ],
    ] {
        let (code, stdout, stderr) = run_in(&dir, args);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        // Located at the finding (the `package` name the required
        // field is missing from), never a bare key.
        assert!(
            stderr.contains("other.package.nml:1:9: error[NML2088]"),
            "{args:?}: {stderr}"
        );
        assert!(
            !stderr.contains("NML2092"),
            "{args:?}: the lint is silent: {stderr}"
        );
    }
}

/// The binding's `layers:` grant is the CLI's own verdict (RFC 0019
/// item 0: one grant provider for both front ends; RFC 0026 B-1 brought
/// the manifest side forward): a `denyRefs` glob that matches the
/// referenced layer's file vetoes the `uses` by rule index (NML2065), an
/// allowlist that admits nothing misses (NML2065, the binding named,
/// never the target), and an allowlist that admits the file composes
/// clean — `nml binding` prints the same rules by the same indices, and
/// the `--json` row carries them. (No fixture carried a grant: the
/// deny/allow arms were pinned in the kernel alone.)
#[test]
fn a_grants_deny_veto_and_allow_miss_reach_the_cli() {
    let grant = |allow: &[&str], deny: &[&str]| {
        let list = |globs: &[&str]| {
            globs
                .iter()
                .map(|g| format!("                - {g:?}\n"))
                .collect::<String>()
        };
        let deny = if deny.is_empty() {
            String::new()
        } else {
            format!("            denyRefs:\n{}", list(deny))
        };
        format!(
            "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
             file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        files:\n            - \
             \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n        strict = true\n        layers:\n            \
             allowRefs:\n{}{deny}    - shared:\n        files:\n            - \"shared/**/*.flow.nml\"\n        \
             schemas:\n            - core\n",
            list(allow)
        )
    };
    let file = "tenants/cu/member-lookup.flow.nml";
    // Deny-veto: the rule's index names it; `binding` prints the rules.
    let dir = workspace_copy("grant-deny-veto");
    std::fs::write(
        dir.join("demo.package.nml"),
        grant(&["tenants/**"], &["tenants/cu/**"]),
    )
    .unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "error[NML2065]: `uses` ref 'base' denied by denyRefs[0] of binding 'tenantFlows' \
             (demo.package.nml) — an operator change, not fixable here; run `nml binding \
             tenants/cu/member-lookup.flow.nml`"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("NML2064"), "{stderr}");
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        &dir.display().to_string(),
        &dir.join(file).display().to_string(),
    ]);
    assert_eq!(code, 1);
    let denial = rows
        .iter()
        .find(|r| r["code"] == "NML2065")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(denial["source"], file, "{denial}");
    assert_eq!(denial["severity"], "error", "{denial}");
    let (code, stdout, _) = run_in(&dir, &["binding", "--root", ".", file]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("layers    granted\n          allowRefs[0] = \"tenants/**\"\n          denyRefs[0] = \"tenants/cu/**\"\n"),
        "{stdout}"
    );
    let (_, rows) = json_rows(&[
        "binding",
        "--json",
        "--root",
        &dir.display().to_string(),
        &dir.join(file).display().to_string(),
    ]);
    let layers = &rows[0]["layers"];
    assert_eq!(layers["granted"], true, "{layers}");
    assert_eq!(
        layers["allowRefs"],
        serde_json::json!(["tenants/**"]),
        "{layers}"
    );
    assert_eq!(
        layers["denyRefs"],
        serde_json::json!(["tenants/cu/**"]),
        "{layers}"
    );
    assert!(layers["maxStackDepth"].is_null(), "{layers}");
    // Allow-miss: the binding is named, never the target.
    let dir = workspace_copy("grant-allow-miss");
    std::fs::write(dir.join("demo.package.nml"), grant(&["shared/**"], &[])).unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "error[NML2065]: `uses` ref 'base' denied: no allowRefs entry of binding \
             'tenantFlows' (demo.package.nml) admits this layer — an operator change, not \
             fixable here; run `nml binding tenants/cu/member-lookup.flow.nml`"
        ),
        "{stderr}"
    );
    // Allowed: the stack composes and validates clean under the binding.
    let dir = workspace_copy("grant-allowed");
    std::fs::write(dir.join("demo.package.nml"), grant(&["tenants/**"], &[])).unwrap();
    let (code, stdout, stderr) = run_in(&dir, &["check", "--root", ".", file]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains(": ok ("), "{stdout}");
    assert!(
        !stderr.contains("NML2064") && !stderr.contains("NML2065"),
        "{stderr}"
    );
}

/// A `layers:` grant breaking a rule of the loader's own is **NML2081**
/// at the universe (RFC 0026 B-1): the row carries the code and the
/// manifest's key, and is LOCATED at the item — `key:line:col:` in the
/// human line, `line`/`col` on the `--json` wire (the sentence names no
/// line: the location is the row's own, stated once); the universe is
/// closed-denied around it exactly as under NML2088 (nothing judged,
/// exit 1, `binding` exits 1 with the row). Three forms — a glob the
/// matcher rejects, a stack cap past the language's, a veto beside an
/// empty allowlist; the block's SHAPE (`maxStackDepth = 0`) stays the
/// meta-schema's NML2088.
#[test]
fn a_layers_grant_breaking_its_own_rules_is_nml2081_at_load() {
    let file = "tenants/cu/member-lookup.flow.nml";
    let with = |block: &str| {
        let manifest =
            std::fs::read_to_string(fixture("workspace").join("demo.package.nml")).unwrap();
        let text = manifest.replace(
            "        strict = true\n",
            &format!("        strict = true\n        layers:\n{block}"),
        );
        assert_ne!(text, manifest, "the fixture's strict binding");
        text
    };
    for (tag, block, (at_line, at_col), wants) in [
        (
            "glob",
            "            allowRefs:\n                - \"tenants/**x\"\n",
            (18, 19),
            "validator 'tenantFlows' layers.allowRefs[0] = \"tenants/**x\": `**` must be a whole segment",
        ),
        (
            "cap",
            "            allowRefs:\n                - \"tenants/**\"\n            maxStackDepth = 17\n",
            (19, 29),
            "validator 'tenantFlows' layers.maxStackDepth = 17 exceeds the language cap 16",
        ),
        (
            "veto",
            "            allowRefs:\n            denyRefs:\n                - \"tenants/cu/**\"\n",
            (18, 13),
            "validator 'tenantFlows' layers.denyRefs has nothing to veto: allowRefs is empty (an empty \
             allowlist already denies every ref)",
        ),
    ] {
        let dir = workspace_copy(&format!("grant-rule-{tag}"));
        std::fs::write(dir.join("demo.package.nml"), with(block)).unwrap();
        let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
        assert_eq!(code, 1, "{tag}: {stderr}");
        let line = format!(
            "demo.package.nml:{at_line}:{at_col}: error[NML2081]: manifest failed to load: \
             {wants}\n"
        );
        assert!(stderr.contains(&line), "{tag}: want {line:?} in {stderr}");
        assert!(
            stderr.contains("for more information, run: nml explain NML2081"),
            "{tag}: {stderr}"
        );
        assert!(
            !stderr.contains("NML2064")
                && !stderr.contains("NML2065")
                && !stderr.contains("NML2088"),
            "{tag}: nothing is judged under an unloadable universe: {stderr}"
        );
        let (code, rows) = json_rows(&[
            "check",
            "--json",
            "--root",
            &dir.display().to_string(),
            &dir.join(file).display().to_string(),
        ]);
        assert_eq!(code, 1, "{tag}");
        let row = rows
            .iter()
            .find(|r| r["code"] == "NML2081")
            .unwrap_or_else(|| panic!("{tag}: {rows:?}"));
        assert_eq!(row["source"], "demo.package.nml", "{tag}: {row}");
        assert_eq!(row["severity"], "error", "{tag}: {row}");
        assert_eq!(
            (row["line"].as_u64(), row["col"].as_u64()),
            (Some(at_line), Some(at_col)),
            "{tag}: located on the wire: {row}"
        );
        assert!(
            row["message"].as_str().unwrap().contains(wants),
            "{tag}: {row}"
        );
        assert!(
            !row["message"].as_str().unwrap().contains("validation at "),
            "{tag}: the sentence names no line: {row}"
        );
        let summary = rows.last().unwrap();
        assert_eq!(summary["closure"], "unloadable", "{tag}: {summary}");
        let (code, stdout, stderr) = run_in(&dir, &["binding", "--root", ".", file]);
        assert_eq!(code, 1, "{tag}: {stdout}{stderr}");
        assert!(
            stderr.contains("error[NML2081]"),
            "{tag}: the universe's word, once, on stderr: {stderr}"
        );
        assert!(
            !stdout.contains("NML2081"),
            "{tag}: never inside a block: {stdout}"
        );
        assert!(!stdout.contains("layers    granted"), "{tag}: {stdout}");
    }
    // The block's shape is the meta-schema's finding: NML2088, as any
    // other malformed manifest field — located at the value, like the
    // loader's own. Wholeness is a shape (`multipleOf = 1`, exact).
    for (tag, depth, wants) in [
        (
            "zero",
            "0",
            "'maxStackDepth' is 0, below the schema's min = 1",
        ),
        (
            "fraction",
            "1.5",
            "'maxStackDepth' is 1.5, not a multiple of the schema's multipleOf = 1 (checked \
             exactly -- no float rounding)",
        ),
    ] {
        let dir = workspace_copy(&format!("grant-rule-shape-{tag}"));
        std::fs::write(
            dir.join("demo.package.nml"),
            with(&format!(
                "            allowRefs:\n                - \"tenants/**\"\n            maxStackDepth = {depth}\n"
            )),
        )
        .unwrap();
        let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", file]);
        assert_eq!(code, 1, "{tag}: {stderr}");
        let line = format!(
            "demo.package.nml:19:29: error[NML2088]: manifest failed to load: \
             {wants}\n"
        );
        assert!(stderr.contains(&line), "{tag}: want {line:?} in {stderr}");
        assert!(!stderr.contains("NML2081"), "{tag}: {stderr}");
    }
}

/// RFC 0026 B-1: the no-grant denial's remedy travels as a `related[]`
/// entry on the wire — the manifest's key, the binding's line and
/// column, the sentence naming the key to admit — so a consumer and an
/// editor land on the binding.
#[test]
fn the_no_grant_denial_carries_a_located_remedy_note_on_the_wire() {
    let (code, rows) = json_rows(&[
        "check",
        "--json",
        "--root",
        "tests/fixtures/workspace",
        "tests/fixtures/workspace/tenants/cu/member-lookup.flow.nml",
    ]);
    assert_eq!(code, 1);
    let denial = rows
        .iter()
        .find(|r| r["code"] == "NML2064")
        .unwrap_or_else(|| panic!("{rows:?}"));
    let related = denial["related"]
        .as_array()
        .unwrap_or_else(|| panic!("{denial}"));
    assert_eq!(related.len(), 1, "{denial}");
    let note = &related[0];
    assert_eq!(note["source"], "demo.package.nml", "{note}");
    assert_eq!(
        (note["line"].as_u64(), note["col"].as_u64()),
        (Some(10), Some(7)),
        "{note}"
    );
    assert_eq!(
        note["message"],
        "to permit it, give this binding a `layers:` grant whose `allowRefs` admits \
         \"tenants/cu/member-lookup.flow.nml\"",
        "{note}"
    );
}

/// NML2093 end to end: the row at the later entry, the `note:` at the
/// first, the explain hint, no fix — with a schema and without one (the
/// structural pass judges a schema-less file too); `--json` carries the
/// located note and no suggestion. One row: the compose pass's, the
/// validator's twin collapsed by the kernel's seeded sink.
#[test]
fn duplicate_entry_is_located_with_the_first_noted_and_no_fix() {
    let dir = scratch_dir("dup-entry");
    let typed = "model thing:\n    v string\n\nthing t:\n    v = \"x\"\n    v = \"y\"\n";
    let bare = "thing t:\n    v = \"x\"\n    v = \"y\"\n";
    std::fs::write(dir.join("typed.nml"), typed).unwrap();
    std::fs::write(dir.join("bare.nml"), bare).unwrap();
    for (name, row_line, note_line) in [("typed.nml", 6, 5), ("bare.nml", 3, 2)] {
        let path = dir.join(name).display().to_string();
        let (code, _, stderr) = run(&["check", &path]);
        assert_eq!(code, 1, "{name}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "{name}:{row_line}:5: error[NML2093]: duplicate entry 'v' — a body declares each \
                 name once\n"
            )),
            "{name}: {stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "{name}:{note_line}:5: note: 'v' first declared here\n"
            )),
            "{name}: {stderr}"
        );
        assert_eq!(
            stderr.matches("error[NML2093]").count(),
            1,
            "{name}: one row, never a twin: {stderr}"
        );
        assert!(
            stderr.contains("for more information, run: nml explain NML2093"),
            "{name}: {stderr}"
        );
        assert!(
            !stderr.contains("help:"),
            "{name}: no remedy rides the row: {stderr}"
        );
        let (code, rows) = json_rows(&["check", "--json", &path]);
        assert_eq!(code, 1);
        let dups: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["code"] == "NML2093").collect();
        assert_eq!(dups.len(), 1, "{name}: {rows:?}");
        let row = dups[0];
        assert_eq!(row["line"], row_line, "{row}");
        assert_eq!(row["col"], 5, "{row}");
        assert_eq!(row["related"][0]["line"], note_line, "{row}");
        assert_eq!(row["related"][0]["col"], 5, "{row}");
        assert_eq!(
            row["related"][0]["message"], "'v' first declared here",
            "{row}"
        );
        assert!(
            row.get("suggestions")
                .is_none_or(|s| s.as_array().is_some_and(Vec::is_empty)),
            "which entry is meant is unknowable: {row}"
        );
    }
}

/// No fix repairs a repeated name: `fix` applies nothing and leaves the
/// bytes alone, `fix --check` fails the gate, and `fmt` keeps both
/// entries in their order — the entries are well-formed.
#[test]
fn a_duplicate_entry_has_no_fix_and_fmt_refuses_the_text_untouched() {
    let dir = scratch_dir("dup-entry-fix");
    let src = "thing t:\n    v = \"x\"\n    v = \"y\"\n";
    let path = dir.join("bare.nml");
    std::fs::write(&path, src).unwrap();
    let p = path.display().to_string();
    let (code, stdout, stderr) = run(&["fix", &p]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
    assert!(
        stdout.contains("1 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), src, "untouched");
    let (code, _, stderr) = run(&["fix", "--check", &p]);
    assert_eq!(code, 1, "an error no fix repairs fails the gate: {stderr}");
    assert!(
        stderr.contains("1 error(s) remain that no fix repairs"),
        "the gate's sentence; the findings are `nml check`'s: {stderr}"
    );
    // A repeated name is a parse finding: the formatter sees only text
    // that parses, so `fmt` reports the row as `parse` and `check` do
    // and writes nothing (a duplicate `#directive`, NML0011, is refused
    // the same way).
    let (code, _, stderr) = run(&["fmt", &p]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("bare.nml:3:5: error[NML2093]: duplicate entry 'v'"),
        "{stderr}"
    );
    assert!(stderr.contains("error: 1 parse error(s)"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        src,
        "fmt writes nothing for text it refuses"
    );
}

/// The file-scope rule (NML1000) is the parse's finding on every verb:
/// `check`, `validate`, `parse` and `fmt` all print the row at the later
/// NAME with the first as a `note:`, `--json` locates both, `fmt` writes
/// nothing — one emission, one shape, every front end.
#[test]
fn a_duplicate_declaration_is_the_parses_finding_on_every_verb() {
    let dir = scratch_dir("dup-decl");
    let src = "service Api:\n    port = 8080\n\nservice Api:\n    port = 9090\n";
    let path = dir.join("dupdecl.nml");
    std::fs::write(&path, src).unwrap();
    let p = path.display().to_string();
    for verb in ["check", "validate", "parse", "fmt"] {
        let (code, _, stderr) = run(&[verb, &p]);
        assert_eq!(code, 1, "{verb}: {stderr}");
        assert!(
            stderr.contains(
                "dupdecl.nml:4:9: error[NML1000]: duplicate declaration 'Api' — a file declares \
                 each name once\n"
            ),
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains("dupdecl.nml:1:9: note: 'Api' first declared here\n"),
            "{verb}: {stderr}"
        );
        assert_eq!(
            stderr.matches("error[NML1000]").count(),
            1,
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains("for more information, run: nml explain NML1000"),
            "{verb}: {stderr}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        src,
        "fmt wrote nothing"
    );
    let (code, rows) = json_rows(&["check", "--json", &p]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML1000")
        .unwrap_or_else(|| panic!("no NML1000 row: {rows:?}"));
    assert_eq!(row["line"], 4, "{row}");
    assert_eq!(row["col"], 9, "{row}");
    assert_eq!(row["related"][0]["line"], 1, "{row}");
    assert_eq!(row["related"][0]["col"], 9, "{row}");
    assert_eq!(
        row["related"][0]["message"], "'Api' first declared here",
        "{row}"
    );
}

/// `nml parse` — the embedder-shaped verb, syntax only — carries both
/// name rules: a text that declares a name twice does not parse.
#[test]
fn nml_parse_reports_a_repeated_name() {
    let dir = scratch_dir("dup-parse");
    let path = dir.join("bare.nml");
    std::fs::write(&path, "thing t:\n    v = \"x\"\n    v = \"y\"\n").unwrap();
    let p = path.display().to_string();
    let (code, stdout, stderr) = run(&["parse", &p]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.is_empty(),
        "no tree is dumped for text that does not parse: {stdout}"
    );
    assert!(
        stderr.contains("bare.nml:3:5: error[NML2093]: duplicate entry 'v'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("bare.nml:2:5: note: 'v' first declared here"),
        "{stderr}"
    );
}

/// The fixer never manufactures a repeated name: `bad.flow.nml`'s
/// did-you-mean (`w` → `v`, beside an existing `v`) would leave a body
/// that does not parse (NML2093), and the re-check gate's first clause
/// — the parse layer never regresses — discards the round. Nothing is
/// written, `--check` fails on what remains, and `--json` carries no
/// applied edit.
#[test]
fn a_did_you_mean_that_would_repeat_a_name_is_refused_by_the_parse_gate() {
    let dir = workspace_copy("fix-no-repeat");
    let root = dir.to_str().unwrap();
    let bad = dir.join("tenants/cu/bad.flow.nml");
    let before = std::fs::read_to_string(&bad).unwrap();
    let p = bad.display().to_string();
    let (code, stdout, stderr) = run(&["fix", "--root", root, &p]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout
            .contains("0 edit(s) applied across 0 of 1 file(s); 2 diagnostic(s) not auto-fixable"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), before, "untouched");
    let (code, _, stderr) = run(&["fix", "--check", "--root", root, &p]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("2 error(s) remain that no fix repairs"),
        "{stderr}"
    );
    let (code, rows) = json_rows(&["fix", "--json", "--dry-run", "--root", root, &p]);
    assert_eq!(code, 0);
    assert!(
        rows.iter()
            .all(|row| row["type"] != "fix" || row["applied"] == 0),
        "no applied edit: {rows:?}"
    );
    assert_eq!(rows.last().unwrap()["edits"], 0, "{rows:?}");
}

/// The `validate` verb judges definition bodies by the same rule — a
/// field defined twice in a model body — and `check` agrees, once.
#[test]
fn validate_verb_judges_definition_bodies_for_repeated_names() {
    let dir = scratch_dir("dup-entry-defs");
    let path = dir.join("defs.model.nml");
    std::fs::write(&path, "model m:\n    a string\n    a number\n").unwrap();
    let p = path.display().to_string();
    for verb in ["validate", "check"] {
        let (code, _, stderr) = run(&[verb, &p]);
        assert_eq!(code, 1, "{verb}: {stderr}");
        assert!(
            stderr.contains(
                "defs.model.nml:3:5: error[NML2093]: duplicate entry 'a' — a body declares each \
                 name once\n"
            ),
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains("defs.model.nml:2:5: note: 'a' first declared here\n"),
            "{verb}: {stderr}"
        );
        assert_eq!(
            stderr.matches("error[NML2093]").count(),
            1,
            "{verb}: {stderr}"
        );
    }
}

/// Composition never trips the rule — an overlay redefining a base
/// property is composition — while a repeat inside the overlay's OWN
/// body is one row, from the pass over the authored body (the merge
/// collapses it, so the validator's pass over the composed view could
/// never see it).
#[test]
fn composition_never_trips_the_entry_rule_but_an_overlays_own_repeat_is_one_row() {
    let dir = scratch_dir("dup-entry-compose");
    let ok = "model thing:\n    v string\n\nthing base:\n    v = \"a\"\n\nthing over uses base:\n    v = \"b\"\n";
    let bad = "model thing:\n    v string\n\nthing base:\n    v = \"a\"\n\nthing over uses base:\n    v = \"b\"\n    v = \"c\"\n";
    std::fs::write(dir.join("ok.nml"), ok).unwrap();
    std::fs::write(dir.join("bad.nml"), bad).unwrap();
    let (code, _, stderr) = run(&["check", &dir.join("ok.nml").display().to_string()]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("NML2093"), "{stderr}");
    let (code, _, stderr) = run(&["check", &dir.join("bad.nml").display().to_string()]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("bad.nml:9:5: error[NML2093]: duplicate entry 'v'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("bad.nml:8:5: note: 'v' first declared here\n"),
        "{stderr}"
    );
    assert_eq!(stderr.matches("error[NML2093]").count(), 1, "{stderr}");
}

/// NML2093 through the meta-schema: the workspace's manifest names
/// `files` twice (a block beside an inline array). The universe is
/// closed-denied — NML2088 located AT the later entry (the row's own line
/// and column; the sentence names no line), the first entry as the row's
/// `note:`, nothing under the manifest validates (the manifest itself is
/// no target), the loader never reads a glob — and `--json` carries the
/// row's `line`/`col` and the note, located in the manifest.
#[test]
fn a_manifest_naming_an_entry_twice_fails_to_load_at_the_later_entry() {
    let root = "tests/fixtures/workspace-dup";
    for target in ["tenants/cu/plain.flow.nml", "demo.package.nml"] {
        let (code, stdout, stderr) = run(&["check", "--root", root, &format!("{root}/{target}")]);
        assert_eq!(code, 1, "{target}: {stdout}{stderr}");
        assert!(
            stderr.contains(
                "demo.package.nml:16:9: error[NML2088]: manifest failed to load: \
                 duplicate entry 'files' — a body declares each name once (`files:` \
                 and `files = …` are two spellings of one entry)\n\
                 demo.package.nml:11:9: note: 'files' first declared here\n"
            ),
            "{target}: {stderr}"
        );
        assert!(
            !stdout.contains(": ok"),
            "{target}: nothing validates: {stdout}"
        );
        assert!(
            !stderr.contains("NML2093"),
            "{target}: the manifest is no target: {stderr}"
        );
    }
    let (code, rows) = json_rows(&[
        "check",
        "--root",
        root,
        "--json",
        &format!("{root}/demo.package.nml"),
    ]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("no NML2088 row: {rows:?}"));
    assert_eq!(row["source"], "demo.package.nml", "{row}");
    assert_eq!(row["line"], 16, "located at the later entry: {row}");
    assert_eq!(row["col"], 9, "{row}");
    assert_eq!(row["related"][0]["source"], "demo.package.nml", "{row}");
    assert_eq!(row["related"][0]["line"], 11, "{row}");
    assert_eq!(row["related"][0]["col"], 9, "{row}");
    assert_eq!(
        row["related"][0]["message"], "'files' first declared here",
        "{row}"
    );
}

/// A row that reports another finding's refusal under a code of its own
/// carries that finding as `cause` on the wire — `{code, source, line,
/// col, message}`, the underlying code a fact beside the row's — on
/// every verb and in `binding`'s `notes[]` alike; the human lines are
/// unchanged. NML2088 over the manifest's first finding (a repeated
/// entry, NML2093: the cause sits where the row sits) and NML2091 over
/// the declared source's first finding (NML0006, in the source's own
/// file, beside the note that already jumps there).
#[test]
fn a_wrapping_row_carries_its_cause_on_the_wire() {
    let root = "tests/fixtures/workspace-dup";
    let target = format!("{root}/tenants/cu/plain.flow.nml");
    let (code, rows) = json_rows(&["check", "--root", root, "--json", &target]);
    assert_eq!(code, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("no NML2088 row: {rows:?}"));
    assert_eq!(
        row["cause"],
        serde_json::json!({
            "code": "NML2093",
            "source": "demo.package.nml",
            "line": 16,
            "col": 9,
            "message": "duplicate entry 'files' — a body declares each name once (`files:` \
                        and `files = …` are two spellings of one entry)",
        }),
        "{row}"
    );
    assert_eq!(
        row["cause"]["line"], row["line"],
        "the cause is where the row is"
    );
    let (_, _, stderr) = run(&["check", "--root", root, &target]);
    assert!(
        stderr.contains("demo.package.nml:16:9: error[NML2088]: manifest failed to load: ")
            && !stderr.contains("cause"),
        "the human line is what it was: {stderr}"
    );
    // NML2091: the cause sits in the source's own file.
    let root = "tests/fixtures/workspace-brokensrc";
    let target = format!("{root}/tenants/cu/plain.flow.nml");
    let cause = serde_json::json!({
        "code": "NML0006",
        "source": "core.model.nml",
        "line": 3,
        "col": 1,
        "message": "indentation of 2 matches no enclosing block (open blocks are at columns 0, 4)",
    });
    for verb in ["check", "validate"] {
        let (code, rows) = json_rows(&[verb, "--root", root, "--json", &target]);
        assert_eq!(code, 1);
        let row = rows
            .iter()
            .find(|r| r["code"] == "NML2091")
            .unwrap_or_else(|| panic!("{verb}: no NML2091 row: {rows:?}"));
        assert_eq!(row["cause"], cause, "{verb}: {row}");
        assert_eq!(
            row["related"][0]["line"], 3,
            "{verb}: the note stays: {row}"
        );
    }
    // `binding --json` carries the row under `notes[]` WHOLE — its cause
    // and its note alike (they rode with empty `related`).
    let (code, rows) = json_rows(&["binding", "--root", root, "--json", &target]);
    assert_eq!(code, 1);
    let binding = rows
        .iter()
        .find(|r| r["type"] == "binding")
        .unwrap_or_else(|| panic!("{rows:?}"));
    let note = binding["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["code"] == "NML2091")
        .unwrap_or_else(|| panic!("{binding}"));
    assert_eq!(note["cause"], cause, "{note}");
    assert_eq!(note["related"][0]["source"], "core.model.nml", "{note}");
    assert_eq!(note["related"][0]["line"], 3, "{note}");
}

/// RFC 0026 decision 3: a failed manifest's first finding rides the
/// NML2088 row WITH its remedy — the did-you-mean in the manifest's file
/// (`suggestions[0].source`, resolved against the text the universe
/// kept), the hint on the human line, the same row under `binding
/// --json` — and `nml fix` reports the edit as pending in the
/// manifest (`routed` on its closing row) while rewriting nothing: the
/// door's rule stands, the tally says where the fix is.
#[test]
fn a_failed_manifests_remedy_rides_the_nml2088_row_and_fix_reports_it_pending() {
    let root = "tests/fixtures/manifest-rules/did-you-mean";
    let target = format!("{root}/tenants/cu/plain.flow.nml");
    let expected = serde_json::json!({
        "kind": "didYouMean",
        "source": "demo.package.nml",
        "edits": [{ "line": 2, "col": 5, "endLine": 2, "endCol": 11, "lines": ["version"] }],
    });
    for verb in ["check", "validate", "fix"] {
        let (exit, rows) = json_rows(&[verb, "--root", root, "--json", &target]);
        assert_eq!(exit, 1, "{verb}");
        let row = rows
            .iter()
            .find(|r| r["code"] == "NML2088")
            .unwrap_or_else(|| panic!("{verb}: no NML2088 row: {rows:?}"));
        assert_eq!(
            row["suggestions"],
            serde_json::json!([expected]),
            "{verb}: {row}"
        );
        assert_eq!(row["cause"]["code"], "NML2001", "{verb}: {row}");
        assert!(
            row["message"]
                .as_str()
                .is_some_and(|m| m.ends_with("unknown property 'versio' (not defined in model 'package') (did you mean \"version\"?)")),
            "{verb}: the hint rides the sentence, after the manifest's `(and N more)` tail: {row}"
        );
        let last = rows.last().unwrap_or_else(|| panic!("{verb}: {rows:?}"));
        assert_eq!(last["type"], "summary", "{verb}: {last}");
        if verb == "fix" {
            assert_eq!(
                last["routed"], 1,
                "the edit is pending in the manifest: {last}"
            );
            assert_eq!(last["edits"], 0, "{last}");
            assert_eq!(last["filesFixed"], 0, "{last}");
            // The door's row is this run's: a real run is never reported
            // as a rehearsal (and a rehearsal says so).
            assert_eq!(last["dryRun"], false, "{last}");
            let (exit, rows) = json_rows(&[verb, "--dry-run", "--root", root, "--json", &target]);
            assert_eq!(exit, 1, "{verb} --dry-run");
            let last = rows.last().unwrap_or_else(|| panic!("{rows:?}"));
            assert_eq!(last["type"], "summary", "{last}");
            assert_eq!(last["dryRun"], true, "{last}");
            assert_eq!(last["routed"], 1, "{last}");
        }
    }
    let (exit, _, stderr) = run(&["check", "--root", root, &target]);
    assert_eq!(exit, 1);
    assert!(
        stderr.contains(
            "demo.package.nml:2:5: error[NML2088]: manifest failed to load \
             (finding 1 of 2): unknown property 'versio' (not defined in model \
             'package') (did you mean \"version\"?)"
        ),
        "{stderr}"
    );
    let manifest =
        std::fs::read_to_string(fixture("manifest-rules/did-you-mean/demo.package.nml")).unwrap();
    assert!(
        manifest.contains("versio = "),
        "the door rewrites nothing: {manifest}"
    );
    // `binding --json` states the universe once, as `diagnostic` rows
    // before the `binding` row — the same row, the same remedy.
    let (exit, rows) = json_rows(&["binding", "--root", root, "--json", &target]);
    assert_eq!(exit, 1);
    let row = rows
        .iter()
        .find(|r| r["type"] == "diagnostic" && r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["suggestions"], serde_json::json!([expected]), "{row}");
}

/// RFC 0026 decision 2: every rule the manifest loader states as a
/// finding has a code of its own, so the NML2088 row carries it as its
/// `cause` on the wire — new code VALUES only: the row's shape, the
/// wire's revision and the human line are as they were (the code rides
/// `cause.code`, never the sentence). One fixture universe per rule
/// under `tests/fixtures/manifest-rules`; the formatVersion gate's cause
/// has no place (`line`/`col` null), every other sits where the row does.
#[test]
fn every_manifest_loader_rule_rides_the_nml2088_row_as_its_cause() {
    let cases = [
        (
            "repeated",
            "NML2094",
            "`[]validator` is declared twice",
            true,
        ),
        (
            "no-package",
            "NML2095",
            "manifest has no `package <name>:` block",
            false,
        ),
        (
            "no-schema",
            "NML2095",
            "manifest declares no `[]schema` sources",
            true,
        ),
        (
            "package-name",
            "NML2096",
            "package name 'Demo' is not a lowercase identifier",
            true,
        ),
        (
            "unnamed",
            "NML2097",
            "`[]schema` entries must be named items",
            true,
        ),
        (
            "empty-binding",
            "NML2098",
            "`[]validator` entry 'flows' needs non-empty `files` and `schemas`",
            true,
        ),
        (
            "undeclared-schema",
            "NML2099",
            "validator 'flows' names schema 'corr'",
            true,
        ),
        (
            "binding-glob",
            "NML2100",
            "validator 'flows' glob 'tenants/**x/*.flow.nml': `**` must be a whole segment",
            true,
        ),
        (
            "budget-units",
            "NML2101",
            "budgetUnits entry \"tenants/**\": segment \"**\"",
            true,
        ),
        (
            "format-version",
            "NML2102",
            "package requires formatVersion 99; this nml supports 1",
            false,
        ),
    ];
    for (rule, code, head, located) in cases {
        let root = format!("tests/fixtures/manifest-rules/{rule}");
        let target = format!("{root}/tenants/cu/plain.flow.nml");
        let (exit, rows) = json_rows(&["check", "--root", &root, "--json", &target]);
        assert_eq!(exit, 1, "{rule}");
        let row = rows
            .iter()
            .find(|r| r["code"] == "NML2088")
            .unwrap_or_else(|| panic!("{rule}: no NML2088 row: {rows:?}"));
        assert_eq!(row["cause"]["code"], code, "{rule}: {row}");
        assert_eq!(row["cause"]["source"], "demo.package.nml", "{rule}: {row}");
        assert!(
            row["cause"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with(head)),
            "{rule}: {row}"
        );
        assert_eq!(
            row["cause"]["line"], row["line"],
            "{rule}: where the row is: {row}"
        );
        assert_eq!(row["line"].is_null(), !located, "{rule}: {row}");
        let (exit, _, stderr) = run(&["check", "--root", &root, &target]);
        assert_eq!(exit, 1, "{rule}");
        assert!(
            stderr.contains("error[NML2088]: manifest failed to load: ")
                && stderr.contains(head)
                && !stderr.contains(code),
            "{rule}: the human line carries the sentence, never the cause's code: {stderr}"
        );
    }
}

/// A row wraps nothing when it IS the finding: reported under the inner
/// finding's own code (NML2081, a `layers:` grant rule), refused by a
/// sentence with no finding behind it (a declared source that is
/// absent). No `cause` key rides those rows, so `cause?.code ?? code`
/// lands on the actionable code every time; a meta-validation finding
/// (NML2001) and a rule of the loader's own shape (a keyword declared
/// twice, NML2094) have codes of their own and ride as the cause, where
/// the row is.
#[test]
fn a_row_that_is_the_finding_carries_no_cause() {
    for (root, code) in [
        ("tests/fixtures/workspace-grant-bad", "NML2081"),
        ("tests/fixtures/workspace-unloadable", "NML2088"),
    ] {
        let target = format!("{root}/tenants/cu/plain.flow.nml");
        let (exit, rows) = json_rows(&["check", "--root", root, "--json", &target]);
        assert_eq!(exit, 1, "{root}");
        let row = rows
            .iter()
            .find(|r| r["code"] == code)
            .unwrap_or_else(|| panic!("{root}: {rows:?}"));
        assert!(row.get("cause").is_none(), "{root}: {row}");
    }
    let dir = workspace_copy("cause-shape");
    let root = dir.display().to_string();
    let target = dir.join("tenants/cu/plain.flow.nml").display().to_string();
    let manifest = std::fs::read_to_string(dir.join("demo.package.nml")).unwrap();
    std::fs::write(
        dir.join("demo.package.nml"),
        format!(
            "{manifest}\n[]validator more:\n    - other:\n        files:\n            - \"x/**\"\n        \
             schemas:\n            - core\n"
        ),
    )
    .unwrap();
    let (exit, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    assert_eq!(exit, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m.contains("`[]validator` is declared twice")),
        "{row}"
    );
    assert_eq!(
        row["cause"]["code"], "NML2094",
        "the manifest's own shape has a code of its own: {row}"
    );
    assert_eq!(row["cause"]["line"], row["line"], "{row}");
    std::fs::write(
        dir.join("demo.package.nml"),
        manifest.replace("version = \"0.1.0\"", "versio = \"0.1.0\""),
    )
    .unwrap();
    let (exit, rows) = json_rows(&["check", "--json", "--root", &root, &target]);
    assert_eq!(exit, 1);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML2088")
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(row["cause"]["code"], "NML2001", "{row}");
    assert_eq!(row["cause"]["source"], "demo.package.nml", "{row}");
    assert_eq!(row["cause"]["line"], row["line"], "where the row is: {row}");
    assert_eq!(row["cause"]["col"], row["col"], "{row}");
    assert!(
        row["cause"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("unknown property 'versio'")),
        "{row}"
    );
}

/// RFC 0019 §Merge policy / RFC 0030: a schema source a package covers is
/// judged under the KERNEL's vocabulary — the language's four merge-policy
/// directives plus the manifest's `[]directive` entries — on every verb that
/// judges definitions: `#sealed` is never unknown, a near-miss of a declared
/// name is NML5000 with its did-you-mean (the sentence the editor shows,
/// byte for byte — the harness pins the same literal), the `--json` row
/// carries the edit, `fix` applies it, and an instance file the package
/// binds is never judged under the vocabulary.
#[test]
fn a_schema_source_is_judged_under_the_kernels_directive_vocabulary_on_every_verb() {
    let dir = fixture("directive-vocabulary");
    const ROW: &str = "core.model.nml:3:22: error[NML5000]: unknown directive '#lvie' (package \
                       'demo') (did you mean \"#live\"?)\n";
    for verb in ["check", "validate"] {
        let (code, _, stderr) = run_in(&dir, &[verb, "--root", ".", "core.model.nml"]);
        assert_eq!(code, 1, "{verb}: {stderr}");
        assert!(stderr.contains(ROW), "{verb}: {stderr}");
        assert_eq!(
            stderr.matches("error[NML5000]").count(),
            1,
            "{verb}: {stderr}"
        );
        assert!(
            !stderr.contains("#sealed"),
            "a language directive is never unknown: {verb}: {stderr}"
        );
    }
    // The instance the package binds validates under the schema and is never
    // judged under the vocabulary (no NML5003 note either).
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", "apps/app.nml"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("NML500"), "{stderr}");
    // The row on the wire: the edit replaces `#lvie` with `#live`.
    let (code, stdout, stderr) =
        run_in(&dir, &["check", "--root", ".", "--json", "core.model.nml"]);
    assert_eq!(code, 1, "{stderr}");
    let rows = rows_of(&stdout);
    let row = rows
        .iter()
        .find(|r| r["code"] == "NML5000")
        .unwrap_or_else(|| panic!("no NML5000 row: {rows:?}"));
    assert_eq!(
        row["message"],
        "unknown directive '#lvie' (package 'demo') (did you mean \"#live\"?)"
    );
    assert_eq!(row["suggestions"][0]["kind"], "didYouMean");
    assert_eq!(
        row["suggestions"][0]["edits"][0],
        serde_json::json!({"col": 22, "endCol": 27, "endLine": 3, "line": 3, "lines": ["#live"]})
    );
    // `fix` applies the did-you-mean — on a copy — and the source then checks clean.
    let scratch = scratch_dir("directive-vocabulary-fix");
    copy_tree(&dir, &scratch);
    let (code, stdout, stderr) = run_in(&scratch, &["fix", "--root", ".", "core.model.nml"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("1 edit(s) applied"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(scratch.join("core.model.nml")).unwrap(),
        "model core:\n    action string #sealed\n    rateLimit number #live\n"
    );
    let (code, _, stderr) = run_in(&scratch, &["check", "--root", ".", "core.model.nml"]);
    assert_eq!(code, 0, "{stderr}");
}

/// A fallback chain ends at its line (RFC 0026 decision 2): `nml check`
/// reports the missing arm ONCE, at the pipe, naming the line break —
/// the next line is the next entry, so nothing else is reported.
#[test]
fn a_fallback_pipe_ending_a_line_is_one_row_at_the_pipe() {
    let dir = scratch_dir("fallback-line");
    std::fs::write(
        dir.join("app.nml"),
        "service App:\n    host = $ENV.HOST |\n    port = 3000\n",
    )
    .unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "app.nml"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "app.nml:2:22: error[NML0002]: expected a value after `|`, found a line break"
        ),
        "{stderr}"
    );
    // The parse band states its own count (`N parse error(s)`): the
    // chain never reached a later band.
    assert!(
        stderr.contains("error: 1 parse error(s)"),
        "one row: {stderr}"
    );
    assert!(
        !stderr.contains("app.nml:3:"),
        "the next line is its own entry: {stderr}"
    );
}

/// A manifest that declares one of the language's merge-policy directives
/// under its own meaning fails to load (NML2082 at the entry), and the
/// universe says so where a file under it is checked — under the rule's
/// OWN code, as the grant's NML2081 rides (RFC 0026 decision 1): the row
/// is the finding, so it carries no `cause`, and the hint names NML2082.
#[test]
fn a_manifest_redeclaring_a_language_directive_is_refused_at_load() {
    let dir = scratch_dir("reserved-directive");
    std::fs::write(
        dir.join("demo.package.nml"),
        "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - core:\n        files:\n            - \"app.nml\"\n        schemas:\n            - core\n\n[]directive directives:\n    - sealed:\n        arg = \"none\"\n        doc = \"Our own seal.\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("core.model.nml"),
        "model core:\n    action string\n",
    )
    .unwrap();
    std::fs::write(dir.join("app.nml"), "core main:\n    action = \"type\"\n").unwrap();
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", "app.nml"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "`[]directive` entry 'sealed' redeclares the language's merge-policy directive \
             `#sealed` — `sealed`, `identity`, `append` and `overlay` are reserved (RFC 0019) \
             and known under every vocabulary; rename it"
        ),
        "{stderr}"
    );
    // The row is the finding, AT THE ENTRY, under its own code; the hint
    // names it; NML2088 appears nowhere (the row is not a wrapper).
    assert!(
        stderr.contains("demo.package.nml:17:7: error[NML2082]: "),
        "located at the entry, under its own code: {stderr}"
    );
    assert!(
        stderr.contains("for more information, run: nml explain NML2082"),
        "the hint names the code the reader acts on: {stderr}"
    );
    assert!(!stderr.contains("NML2088"), "no wrapper: {stderr}");
    let (code, stdout, _) = run_in(&dir, &["check", "--json", "--root", ".", "app.nml"]);
    assert_eq!(code, 1, "{stdout}");
    let rows = rows_of(&stdout);
    let row = rows
        .iter()
        .find(|r| r["type"] == "diagnostic")
        .unwrap_or_else(|| panic!("no diagnostic row: {rows:?}"));
    assert_eq!(row["code"], "NML2082", "{row}");
    assert_eq!(row["line"], 17, "{row}");
    assert_eq!(row["col"], 7, "{row}");
    assert_eq!(row["source"], "demo.package.nml", "{row}");
    assert!(
        row["cause"].is_null(),
        "a row that is the finding carries no cause: {row}"
    );
    assert!(
        !rows.iter().any(|r| r["code"] == "NML2088"),
        "no wrapper row: {rows:?}"
    );
}

/// The kernel judge's OTHER verdicts reach `nml check` as rows (RFC 0026
/// B-22): a declared directive given an argument it does not take (NML5001),
/// `#live` beside `#restart` when the package declares both (NML5002), and
/// the advisory note for a schema source beside the manifest with no
/// `[]schema` entry (NML5003) — an info, so `-q` drops it and the counts
/// stay exact; every row rides the wire with its code.
#[test]
fn the_clis_directive_rows_carry_arity_conflict_and_the_sibling_note() {
    let dir = fixture("directive-vocabulary-sibling");
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", "stray.model.nml"]);
    assert_eq!(code, 1, "{stderr}");
    for line in [
        "stray.model.nml:2:17: error[NML5001]: '#live' takes no argument",
        "stray.model.nml:3:23: error[NML5002]: '#live' and '#restart' contradict — pick one",
        "stray.model.nml:1:1: info[NML5003]: not part of package 'demo'; add a []schema entry to participate",
        "error: 2 error(s)",
    ] {
        assert!(stderr.contains(line), "{line:?} in {stderr}");
    }
    let (code, _, quiet) = run_in(&dir, &["check", "-q", "--root", ".", "stray.model.nml"]);
    assert_eq!(code, 1, "{quiet}");
    assert!(
        quiet.contains("error[NML5001]")
            && quiet.contains("error[NML5002]")
            && !quiet.contains("NML5003"),
        "-q keeps the errors and drops the note: {quiet}"
    );
    let (code, stdout, _) = run_in(&dir, &["check", "--json", "--root", ".", "stray.model.nml"]);
    assert_eq!(code, 1, "{stdout}");
    let codes: Vec<String> = rows_of(&stdout)
        .iter()
        .filter(|r| r["type"] == "diagnostic")
        .map(|r| r["code"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(codes, ["NML5001", "NML5002", "NML5003"], "{stdout}");
    let (code, _, stderr) = run_in(&dir, &["check", "--root", ".", "core.model.nml"]);
    assert_eq!(
        code, 0,
        "a declared source under the same vocabulary is clean: {stderr}"
    );
}

/// Two root-level packages that could each cover an undeclared schema
/// source (neither declares it) are an ambiguity `check` and `validate`
/// SAY — one info row at the top of the file naming both packages, the
/// directives judged under no vocabulary (exit 0: an unknown directive is
/// accepted, as under no package) — never a silent pass: with one package
/// the same file is NML5000. Under `--json` the row is an uncoded info
/// diagnostic; `-q` prints nothing.
#[test]
fn an_undeclared_schema_source_two_packages_could_cover_says_its_coverage_is_ambiguous() {
    let dir = scratch_dir("ambiguous-coverage");
    std::fs::create_dir_all(dir.join("tenants/cu")).unwrap();
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    std::fs::write(dir.join("core.model.nml"), "model thing:\n    v string\n").unwrap();
    std::fs::write(
        dir.join("stray.model.nml"),
        "model stray:\n    v string #bogus\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tenants/cu/plain.flow.nml"),
        "thing a:\n    v = \"x\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("shared/s.flow.nml"), "thing a:\n    v = \"x\"\n").unwrap();
    let manifest = |name: &str, glob: &str| {
        format!(
            "package {name}:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    \
             - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - flows:\n        \
             files:\n            - \"{glob}\"\n        schemas:\n            - core\n"
        )
    };
    std::fs::write(
        dir.join("demo.package.nml"),
        manifest("demo", "tenants/**/*.flow.nml"),
    )
    .unwrap();
    std::fs::write(
        dir.join("other.package.nml"),
        manifest("other", "shared/**/*.flow.nml"),
    )
    .unwrap();
    let note = "stray.model.nml:1:1: info: package coverage ambiguous: 2 packages could cover this \
                schema source (demo, other) and none declares it — judged under no vocabulary \
                (every directive accepted); declare it in one package's []schema";
    for verb in ["check", "validate"] {
        let (code, _stdout, stderr) = run_in(&dir, &[verb, "--root", ".", "stray.model.nml"]);
        assert_eq!(
            code, 0,
            "{verb}: judged under no vocabulary is not a failure: {stderr}"
        );
        assert!(
            stderr.contains(note),
            "{verb}: the ambiguity is said: {stderr}"
        );
        assert!(
            !stderr.contains("NML5000"),
            "{verb}: no vocabulary, no verdict: {stderr}"
        );
        let (code, stdout, stderr) =
            run_in(&dir, &[verb, "--json", "--root", ".", "stray.model.nml"]);
        assert_eq!(code, 0, "{verb} --json: {stderr}");
        let rows = rows_of(&stdout);
        let row = rows
            .iter()
            .find(|r| r["type"] == "diagnostic")
            .unwrap_or_else(|| panic!("{verb}: no diagnostic row: {rows:?}"));
        assert_eq!(row["severity"], "info", "{row}");
        assert!(row["code"].is_null(), "uncoded, like the editor's: {row}");
        assert_eq!(row["line"], 1, "{row}");
        assert!(
            row["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("package coverage ambiguous: 2 packages")),
            "{row}"
        );
        let (code, stdout, stderr) = run_in(&dir, &[verb, "-q", "--root", ".", "stray.model.nml"]);
        assert_eq!(code, 0);
        assert!(
            stdout.is_empty() && stderr.is_empty(),
            "{verb} -q: {stdout}{stderr}"
        );
    }
    // One package: the same source is covered and judged.
    std::fs::remove_file(dir.join("other.package.nml")).unwrap();
    let (code, _stdout, stderr) = run_in(&dir, &["check", "--root", ".", "stray.model.nml"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error[NML5000]: unknown directive '#bogus' (package 'demo')"),
        "{stderr}"
    );
    assert!(!stderr.contains("package coverage"), "{stderr}");
}
