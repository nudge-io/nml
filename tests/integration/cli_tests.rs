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
            "tests/fixtures/schema-check/widget-missing-name.nml",
        ])
        .output()
        .expect("failed to run nml");

    assert!(
        !output.status.success(),
        "check should fail when an inherited required field is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing required field 'name'"),
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
