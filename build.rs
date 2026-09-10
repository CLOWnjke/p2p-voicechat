fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // Иконка самого exe — это ресурс Windows, его встраивает компоновщик.
    // Для этого нужен rc.exe из Windows SDK; если его нет, сборку это
    // ронять не должно — иконка не тот повод останавливать всё остальное.
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=иконка в exe не встроилась: {e}");
        }
    }
}
