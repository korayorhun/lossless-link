fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "Lossless Link");
        res.set("ProductName", "Lossless Link");
        res.set("LegalCopyright", "MIT License");
        res.compile().expect("compiling Windows resources");
    }
}
