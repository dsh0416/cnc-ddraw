use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("crate should live under crates/ddraw-dll");
    let exports = repo_root.join("exports.def");

    println!("cargo:rerun-if-changed={}", exports.display());
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("res.rc").display()
    );

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/DEF:{}", exports.display());
    }

    #[cfg(windows)]
    {
        let resource = repo_root.join("res.rc");
        if resource.exists() {
            embed_resource::compile(resource);
        }
    }
}
