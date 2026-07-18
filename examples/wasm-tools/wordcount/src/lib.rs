//! A pure-compute Kelter WASM tool: word statistics.
//!
//! Guest ABI (core wasm, minimal byte protocol — no WASI, no component
//! model): the host writes the input string into linear memory, calls
//! `run(ptr, len)`, and reads a JSON result string back from the returned
//! (ptr<<32 | len). This module imports NOTHING, so by construction it has
//! no access to the network, filesystem, clock or environment — the
//! capability sandbox is the absence of host imports.

use std::alloc::{alloc as rust_alloc, Layout};

/// Allocate `len` bytes of guest memory and return the offset. The host
/// calls this to obtain a buffer, then writes the input there.
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = Layout::from_size_align(len as usize, 1).unwrap();
    // Leaked on purpose: the guest is a one-shot compute per instance; the
    // host tears the instance down after reading the result.
    unsafe { rust_alloc(layout) as i32 }
}

/// Compute word statistics for the UTF-8 input at [ptr, ptr+len).
/// Returns a JSON string `{"words":N,"chars":M,"lines":L}` packed as
/// (out_ptr as u64) << 32 | (out_len as u64).
#[no_mangle]
pub extern "C" fn run(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let text = std::str::from_utf8(input).unwrap_or("");

    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    let lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };

    let json = format!("{{\"words\":{words},\"chars\":{chars},\"lines\":{lines}}}");
    let bytes = json.into_bytes();
    let out_len = bytes.len();
    let out_ptr = bytes.as_ptr() as i32;
    std::mem::forget(bytes); // host reads it, then drops the instance
    ((out_ptr as i64) << 32) | (out_len as i64)
}
