fn main() {
    println!("cargo:rerun-if-changed=assets/rustplay.rc");
    println!("cargo:rerun-if-changed=assets/rustplay.ico");
    println!("cargo:rerun-if-changed=assets/rustplay.manifest");
    embed_resource::compile("assets/rustplay.rc", embed_resource::NONE)
        .manifest_required()
        .expect("no se pudieron compilar los recursos de RustPlay");
}
