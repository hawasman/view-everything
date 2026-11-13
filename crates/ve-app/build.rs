//! Embeds the app icon in the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/view-everything.rc");
    println!("cargo:rerun-if-changed=../../packaging/icons/view-everything.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../packaging/windows/view-everything.rc", embed_resource::NONE)
            .manifest_required()
            .expect("embed the app icon resource");
    }
}
