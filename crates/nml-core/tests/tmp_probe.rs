#[test]
fn probe() {
    let src = "deploy:\n    drain_timeout = $ENV.D\n";
    println!(
        "parse: {:?}",
        nml_core::parse(src).map(|f| f.declarations.len())
    );
    println!(
        "cst  : {:?}",
        nml_core::cst::parse_to_ast(src).map(|f| f.declarations.len())
    );
}
