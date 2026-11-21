fn main() {
    // Unicorn's QEMU core uses 128-bit atomics provided by libatomic. A link
    // arg (not link-lib) so it lands after the unicorn archives.
    println!("cargo:rustc-link-arg-tests=-latomic");
}
