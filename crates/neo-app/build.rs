// Embeds the app icon (resource ID 1, which GPUI uses for the window and
// taskbar) and version info. Windows shows the FileDescription ("Neo") as
// the source of Neo's notifications.
fn main() {
    println!("cargo:rerun-if-changed=../../assets/neo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("../../assets/neo.ico", "1");
        res.set("FileDescription", "Neo");
        res.set("ProductName", "Neo");
        res.set("CompanyName", "Neo");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
