//! The build script of every test firmware (`build = "../build.rs"`): links
//! with the shared `link.ld`.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/../link.ld");
    println!("cargo:rerun-if-changed=../link.ld");
}
