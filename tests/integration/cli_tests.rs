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
    // externally tagged, magnitude bare, unit as its source suffix.
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
        &serde_json::json!({"Duration": {"magnitude": 30, "unit": "s"}}),
        "wire shape drifted: {value}"
    );
    for unit in ["\"s\"", "\"ms\"", "\"h\"", "\"m\""] {
        assert!(stdout.contains(unit), "missing unit {unit}");
    }
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
    let temp = std::env::temp_dir().join("nml_fmt_test.nml");
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

/// The fixer's structural-injection guard: a suggestion whose replacement
/// embeds decoded user content containing a line break (here the
/// role-literal fix for a string authored with `\n` escapes) is refused —
/// file content must never smuggle new structure through an auto-applied
/// edit. The file stays byte-identical.
#[test]
fn test_fix_refuses_replacements_with_control_characters() {
    let dir = std::env::temp_dir().join(format!("nml_fix_inject_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = dir.join("inject.nml");
    let source = "model svc:\n    owner role\n\nsvc A:\n    owner = \"admin\\n    evil = 1\"\n";
    std::fs::write(&file, source).expect("write");
    let output = nml_bin()
        .args(["fix", file.to_str().unwrap()])
        .output()
        .expect("run nml");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 edit(s) applied"), "{stdout}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
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
