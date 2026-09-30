//! UniFFI's bindings generator, built from this workspace so that it is always
//! the same UniFFI version as `leal-ffi`. `just ffi` runs it to write the Swift
//! bindings into `app/Generated/`.

fn main() {
    uniffi::uniffi_bindgen_main();
}
