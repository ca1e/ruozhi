fn main() {
    let target = std::env::var("TARGET").unwrap();

    // c/fenster.h is a vendored copy of https://github.com/zserge/fenster with
    // one local patch (see the comment at the top of the file).
    cc::Build::new()
        .file("c/fenster.c")
        .include("c")
        .warnings(false)
        .compile("fenster");

    if target.contains("darwin") {
        println!("cargo:rustc-link-lib=framework=Cocoa");
    } else if target.contains("linux") {
        println!("cargo:rustc-link-lib=X11");
    } else if target.contains("windows") {
        // fenster's win32 backend: window/input APIs live in user32, the
        // blit in gdi32
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=gdi32");
        embed_windows_resources();
    }
    println!("cargo:rerun-if-changed=c/fenster.c");
    println!("cargo:rerun-if-changed=c/fenster.h");
    println!("cargo:rerun-if-changed=build.rs");
    if target.contains("windows") {
        println!("cargo:rerun-if-changed=assets/ruozhi.ico");
    }
}

/// Embed the icon + VERSIONINFO into the exe so Explorer / the taskbar / the
/// volume mixer show the ruozhi identity (macOS gets this from Info.plist in
/// make_app.sh instead).
#[cfg(target_os = "windows")]
fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/ruozhi.ico");
    res.set("FileDescription", "ruozhi — xiaozhi voice client");
    res.set("ProductName", "ruozhi");
    res.set(
        "FileVersion",
        // VERSIONINFO wants a four-part number: "0.1.0" -> "0.1.0.0"
        &format!("{}.0", env!("CARGO_PKG_VERSION")),
    );
    if let Err(e) = res.compile() {
        // keep the build usable (icon is cosmetic) but make the cause visible
        println!("cargo:warning=icon/resource embedding failed: {e}");
    }
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_resources() {}
