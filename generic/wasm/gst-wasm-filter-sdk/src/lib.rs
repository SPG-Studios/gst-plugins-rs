//pub use once_cell::sync::Lazy;
//pub use serde::{Deserialize, Serialize};
//pub use std::collections::HashMap;
//pub use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod deps {
    pub use once_cell;
    pub use serde_json;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caps {
    pub media_type: String,
    #[serde(flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

pub trait WasmModuleImpl: Sized + Send + 'static {
    fn new() -> Self;
    fn set_caps(&mut self, caps: &Caps);
    fn transform_caps(&self, caps: &Caps) -> Option<Caps> {
        Some(caps.to_owned())
    }
    fn process_buffer(&mut self, input: &[u8], output: &mut [u8]) -> i32;
}

#[macro_export]
macro_rules! define_wasm_module {
    ($module_type:ty) => {
        use ::std::alloc::{Layout, alloc, dealloc};
        use ::std::os::raw::c_void;
        use ::std::slice;
        use ::std::str;

        static FILTER_INSTANCE: $crate::deps::once_cell::sync::Lazy<
            ::std::sync::Mutex<$module_type>,
        > = $crate::deps::once_cell::sync::Lazy::new(|| {
            ::std::sync::Mutex::new(<$module_type as WasmModuleImpl>::new())
        });

        static mut RETURN_BUFFER: Vec<u8> = Vec::new();

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn allocate(size: u32) -> *mut c_void {
            let layout = Layout::from_size_align(size as usize, 1).unwrap();
            alloc(layout) as *mut c_void
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn deallocate(ptr: *mut c_void, size: u32) {
            let layout = Layout::from_size_align(size as usize, 1).unwrap();
            dealloc(ptr as *mut u8, layout);
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn set_caps(ptr: *const u8, len: u32) {
            let json_bytes = slice::from_raw_parts(ptr, len as usize);
            if let Ok(json_str) = str::from_utf8(json_bytes) {
                if let Ok(caps) = $crate::deps::serde_json::from_str::<Caps>(json_str) {
                    FILTER_INSTANCE.lock().unwrap().set_caps(&caps);
                }
            }
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn transform_caps(ptr: *const u8, len: u32) -> i64 {
            let json_bytes = slice::from_raw_parts(ptr, len as usize);
            let json_str = match str::from_utf8(json_bytes) {
                Ok(s) => s,
                Err(_) => return 0,
            };

            let caps = match $crate::deps::serde_json::from_str::<Caps>(json_str) {
                Ok(c) => c,
                Err(_) => return 0,
            };

            let result_caps = FILTER_INSTANCE.lock().unwrap().transform_caps(&caps);
            if let Some(caps) = result_caps {
                if let Ok(json_string) = $crate::deps::serde_json::to_string(&caps) {
                    RETURN_BUFFER.clear();
                    RETURN_BUFFER.extend_from_slice(json_string.as_bytes());

                    let out_ptr = RETURN_BUFFER.as_ptr() as u32;
                    let out_len = RETURN_BUFFER.len() as u32;
                    return ((out_ptr as i64) << 32 | (out_len as i64));
                }
            }
            0
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn process_buffer(
            in_ptr: *const u8,
            in_len: u32,
            out_ptr: *mut u8,
            out_len: u32,
        ) -> i32 {
            let input_slice = slice::from_raw_parts(in_ptr, in_len as usize);
            let output_slice = slice::from_raw_parts_mut(out_ptr, out_len as usize);

            FILTER_INSTANCE
                .lock()
                .unwrap()
                .process_buffer(input_slice, output_slice)
        }
    };
}
