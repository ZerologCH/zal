// Copy the VM's language suite for tests/language_x64.rs, pointing its
// `root()` at the repository root (this crate sits two levels deep).
fn main() {
    let src = "../../vm/tests/language.rs";
    println!("cargo:rerun-if-changed={src}");
    let text = std::fs::read_to_string(src).expect("vm/tests/language.rs");
    let from = r#"PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")"#;
    assert!(text.contains(from), "language.rs root() changed; update build.rs");
    let text = text.replace(from, r#"PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")"#);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("language.rs");
    std::fs::write(out, text).unwrap();
}
