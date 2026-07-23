//! Embeds the application icon into the Windows executable so it shows up in
//! Explorer, the taskbar and the title bar. Best-effort: if no resource
//! compiler is available the build still succeeds (the runtime window icon set
//! via eframe still applies).

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/soundcloud.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/soundcloud.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed executable icon: {e}");
        }
    }
}
