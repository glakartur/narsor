//! Rebuild the crate when a file of the application changes: the files are
//! compiled in by `include_dir!`, which does not tell Cargo about them.
fn main() {
    println!("cargo:rerun-if-changed=ui");
}
