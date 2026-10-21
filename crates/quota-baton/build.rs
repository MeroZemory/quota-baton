fn main() {
    println!("cargo:rerun-if-changed=native/plain_header_seh.c");
    println!("cargo:rerun-if-changed=native/plain_header_seh.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        cc::Build::new()
            .file("native/plain_header_seh.c")
            .include("native")
            .flag("/TC")
            .warnings(true)
            .compile("qb_plain_header_seh");
    }
}
