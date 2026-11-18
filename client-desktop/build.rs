//! Windows: the program's icon and its manifest.
//!
//! The manifest asks for administrator rights. A process without them
//! cannot send keys or clicks to windows of elevated programs (User
//! Interface Privilege Isolation): Task Manager, installers, an elevated
//! terminal would ignore the person remoting this computer. The host agent
//! is a child of this program and inherits its rights. What an elevated
//! process still cannot reach is the secure desktop (the consent prompt
//! itself, the sign-in screen); that takes a service.
//!
//! Starting with Windows is then done by a scheduled task
//! (client-qml/src/win_autostart.rs): the Run key does not start programs
//! that need elevation.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=windows/app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("icons/icon.ico");
    res.set_manifest_file("windows/app.manifest");
    res.set("ProductName", "Lunaris Client");
    res.set("FileDescription", "Lunaris Client");
    if let Err(e) = res.compile() {
        panic!("The Windows resources (icon, manifest) could not be compiled: {}", e);
    }
}
