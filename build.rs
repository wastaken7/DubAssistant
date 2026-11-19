fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "DubAssistant");
        res.set(
            "FileDescription",
            "DubAssistant - Extração, organização e upload de dublagens",
        );
        if let Err(e) = res.compile() {
            panic!("Falha ao embutir ícone Windows no executável: {e}");
        }
    }
}
