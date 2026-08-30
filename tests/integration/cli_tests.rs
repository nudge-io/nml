use std::path::Path;
use std::process::Command;

fn nml_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nml"));
    // Integration tests run from the nml-cli dir; set cwd to workspace root
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    cmd.current_dir(workspace_root);
    cmd
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("USAGE"));
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
    assert!(
        stderr.contains("decimal") || stderr.contains("precision") || stderr.contains("error"),
        "stderr should mention the precision error: {stderr}"
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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

#[test]
fn wide_uses_clause_is_rejected_quickly() {
    // Security: the 16-layer cap must reject BEFORE the C3 merge — a
    // multi-thousand-ref clause used to buy minutes of cubic CPU from
    // kilobytes of input before the post-merge depth check saw it.
    let dir = std::env::temp_dir().join("nml-layers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wide-clause.nml");
    let mut src = String::from(
        "model thing:\n    v string\n\nthing a:\n    v = \"a\"\n\nthing b:\n    v = \"b\"\n\n",
    );
    for i in 0..1500 {
        src.push_str(&format!("thing base{i} uses a, b:\n    v = \"x\"\n\n"));
    }
    src.push_str("thing top uses ");
    src.push_str(
        &(0..1500)
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
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "rejection is pre-merge and near-linear, took {elapsed:?}"
    );
}

#[test]
fn validate_flags_uses_on_schema_definitions() {
    // NML2062's schema-definition form is definition-intrinsic, so
    // `validate` owns it with `check`'s exact wording.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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

#[test]
fn wide_model_compose_is_not_quadratic() {
    // Security: per-entry linear scans of wide models made compose
    // O(width²) across layers — tens of seconds from a sub-megabyte
    // hostile file. Field lookups are mapped now; a wide fully-populated
    // stack must compose fast.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
fn wide_union_bodies_compose_fast() {
    // Security: every union position folds variant decisions and (on a
    // switch) normalizes the displaced group for the seal scan — a wide
    // stack of union fields with per-layer switches must stay
    // near-linear, or hostile sub-megabyte input buys seconds of CPU.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
fn wide_union_rejected_switches_compose_fast() {
    // The seal-scan axis the unsealed wide-union pin cannot guard: every
    // switch normalizes the displaced group for judgment — with seals
    // ASSIGNED, so the scan actually runs, width × layers times.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let out = nml_bin()
        .args(["check", f.to_str().unwrap()])
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
fn wide_list_variant_rejected_switches_scale_linearly_in_items() {
    // The Items-establishment axis: N sealed list items displaced by M
    // rejected switches was N×M full scans with an O(hits²) dedup —
    // super-linear (~3× per doubling) from a sub-megabyte file. The
    // judgment is memoized per unchanged group and dedups by hash now.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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

#[test]
fn large_identity_lists_compose_fast() {
    // Security: per-item linear scans over the resolved list and the
    // sibling item pool were O(items²) — seconds of CPU from sub-megabyte
    // hostile lists. Both lookups are bucketed now.
    let dir = std::env::temp_dir().join("nml-layers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("large-list.nml");
    let n = 8000;
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
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "item merge and seal scan are near-linear, took {elapsed:?}"
    );
}

#[test]
fn base_defect_reports_once_across_overlays() {
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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

#[test]
fn the_nml2054_shape_draws_nml2042_not_nml2085() {
    // An arm model declares a union FIELD named like the discriminator
    // (the NML2054 advisory shape). The base supplies the field's body
    // (`kind as va2:`); the dependent states `kind = 5`. Stripping by
    // name hides the non-string entry from the union field, so the
    // truthful finding is NML2042 at the dependent — the field can
    // never be set — not a whole-value-spelling NML2085 discard.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    assert_eq!(stderr.matches("error[NML2042]").count(), 1, "{stderr}");
    assert!(!stderr.contains("NML2085"), "no discard verdict: {stderr}");
    let kind5 = src[..src.find("kind = 5").unwrap()].lines().count();
    assert!(
        stderr.contains(&format!(":{kind5}:")),
        "at the dependent: {stderr}"
    );
}

#[test]
fn a_non_string_restatement_draws_two_nml2042_and_no_dead_delta() {
    // `kind = 5` over `kind = 5`: both pass through — nothing overlays,
    // so the NML2084 dead-delta cannot fire — and the every-entry check
    // reports each at its author's span.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
        let run = |label: &str| -> String {
            let out = nml_bin()
                .args([
                    "fix",
                    "--schema",
                    tmp.to_str().unwrap(),
                    target.to_str().unwrap(),
                ])
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
fn fix_reports_budget_exhaustion_and_a_second_run_finishes() {
    // The 64-round clamp, exercised for real: 72 restating dependents
    // beside 12 suppressed (mid, top-uses-mid,base) pairs sit safely on
    // the exhaustion plateau (the cliff shapes flip on any fixer
    // improvement — do not shrink this toward (63, 1)). Run 1 exhausts:
    // the stderr note prints EXACTLY once and stdout's summary carries
    // the budget suffix instead of mislabeling landable candidates.
    // Run 2 converges silently; run 3 is the literal fixpoint. Edit
    // counts are fixer-internal and deliberately unasserted.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join(format!("nml-hostile-name-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fix_never_deletes_a_shared_blocks_distributed_row() {
    // The block-form `.shared` corruption vector: `.retry:` distributes
    // REAL CST rows into every item, so an NML2060 equal-value deletion
    // on ONE item's redundancy would locate the real `max = "3"` row and
    // strip the default from EVERY item — silently, since `nml check`
    // passes afterward. The resolver refuses any target inside a
    // `.shared` body (`SharedDistribution`), printed, file untouched.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
fn fix_leaves_a_cr_terminated_file_byte_identical() {
    // A bare CR in token position has NO machine fix: on a CR-terminated
    // ("old Mac") file every CR is a line ending, and deleting it glues
    // the lines together (`service Api:\r    port = 8080\r` became
    // `service Api:    port = 8080`, and the shrinking finding count
    // ACCEPTED the round). The file is reported, never rewritten.
    let dir = std::env::temp_dir().join("nml-layers-test");
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
    let dir = std::env::temp_dir().join("nml-layers-test");
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
