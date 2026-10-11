//! Links each program's icon (icons/<program>.res, made by onelog's
//! tools/make-icons) into its exe. MSVC only: a .res is a Windows resource.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    for program in ["abixio"] {
        println!("cargo:rerun-if-changed=icons/{program}.res");
        println!("cargo:rustc-link-arg-bin={program}={dir}/icons/{program}.res");
    }
}
