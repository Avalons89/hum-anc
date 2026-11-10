fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("assets/hum-anc.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
