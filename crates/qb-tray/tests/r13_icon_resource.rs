#![cfg(windows)]

#[path = "../../../build-support/tests/pe_icon.rs"]
mod pe_icon;

#[test]
fn r13_executable_icon_resource_matches_shared_generator() {
    pe_icon::assert_executable(env!("CARGO_BIN_EXE_quota-baton-tray"));
}

#[test]
fn r13_executable_icon_decoder_rejects_corruption() {
    pe_icon::assert_rejects_corruption(env!("CARGO_BIN_EXE_quota-baton-tray"));
}
