fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    // Icon and name inside idm2.exe, for Explorer, Task Manager (incl. Startup apps) and the taskbar.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("ui/idm2.ico")
            .set("ProductName", "IDM2")
            .set("FileDescription", "IDM2")
            .set("InternalName", "idm2")
            .set("OriginalFilename", "idm2.exe")
            .compile()
            .unwrap();
    }
}
