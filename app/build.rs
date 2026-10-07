fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=discostu.rc");
        println!("cargo:rerun-if-changed=assets/discostu.ico");
        embed_resource::compile("discostu.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embed app icon");
    }
}
