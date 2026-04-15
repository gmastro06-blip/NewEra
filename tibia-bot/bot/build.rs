// build.rs del bot
// El crate `ndi` busca NDI_SDK_DIR automáticamente, pero re-exportamos
// el hint de linkeo por si hay quirks con la ruta del SDK de Vizrt/NewTek.
fn main() {
    if let Ok(sdk) = std::env::var("NDI_SDK_DIR") {
        // Linux x86_64: la lib está en <SDK>/lib/x86_64-linux-gnu/
        println!("cargo:rustc-link-search=native={}/lib/x86_64-linux-gnu", sdk);
        println!("cargo:rustc-link-lib=ndi");
    }
    // Si NDI_SDK_DIR no está seteado, el crate `ndi` emitirá un error claro
    // durante la compilación indicando dónde descargar el SDK.
}
