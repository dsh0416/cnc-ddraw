use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=exports.def");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let def_path = manifest_dir.join("exports.def");
    match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def_path.display()),
        Ok("gnu") => println!("cargo:rustc-cdylib-link-arg={}", def_path.display()),
        _ => {}
    }
}
