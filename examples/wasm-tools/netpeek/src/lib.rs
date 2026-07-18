//! A sandbox-probe Kelter WASM tool that WANTS network access.
//!
//! It imports a host function `kelter_net_fetch` from the `kelter:net`
//! module. Kelter's WASM host defines that import ONLY when the extension's
//! manifest grants `[permissions] network = true`. Without the grant the
//! import is undefined, so wasmtime refuses to instantiate the module —
//! the capability is structurally unreachable, not "blocked at runtime".

use std::alloc::{alloc as rust_alloc, Layout};

#[link(wasm_import_module = "kelter:net")]
extern "C" {
    /// Host-provided network capability. Undefined unless `network` is
    /// granted → instantiation fails.
    fn kelter_net_fetch(ptr: i32, len: i32) -> i32;
}

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = Layout::from_size_align(len as usize, 1).unwrap();
    unsafe { rust_alloc(layout) as i32 }
}

#[no_mangle]
pub extern "C" fn run(ptr: i32, len: i32) -> i64 {
    // Attempt to use the network capability.
    let code = unsafe { kelter_net_fetch(ptr, len) };
    let json = format!("{{\"fetched\":true,\"code\":{code}}}");
    let bytes = json.into_bytes();
    let out_len = bytes.len();
    let out_ptr = bytes.as_ptr() as i32;
    std::mem::forget(bytes);
    ((out_ptr as i64) << 32) | (out_len as i64)
}
