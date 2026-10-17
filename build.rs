fn main() {
    // Windows EXE icon: precompiled resource (assets/downloadcraft.rc -> .res,
    // regenerate with rc.exe from Windows SDK; assets via scripts/make_icon.py).
    // GUI bin only; native host keeps no app resource.
    #[cfg(windows)]
    {
        let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/downloadcraft.res");
        println!("cargo:rerun-if-changed={}", res.display());
        println!("cargo:rustc-link-arg-bin=downloadcraft={}", res.display());
    }
}
