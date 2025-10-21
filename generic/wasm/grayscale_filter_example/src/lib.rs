use std::alloc::{Layout, alloc, dealloc};
use std::slice;
use std::str;

#[derive(Clone, Copy)]
enum FilterMode {
    Grayscale,
    Sepia,
    RedChannel,
}

static mut MODE: FilterMode = FilterMode::Grayscale;
static mut CAPS: Option<Caps> = None;

#[derive(serde::Deserialize, Debug, Clone)]
struct Caps {
    format: String,
    width: u32,
    height: u32,
}

#[unsafe(no_mangle)]
pub extern "C" fn allocate(size: u32) -> *mut u8 {
    let layout = Layout::from_size_align(size as usize, 1).unwrap();
    unsafe { alloc(layout) }
}

#[unsafe(no_mangle)]
pub extern "C" fn deallocate(ptr: *mut u8, size: u32) {
    let layout = Layout::from_size_align(size as usize, 1).unwrap();
    unsafe { dealloc(ptr, layout) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn configure(config_ptr: *const u8, config_len: u32) {
    let config_bytes = slice::from_raw_parts(config_ptr, config_len as usize);
    let config_str = str::from_utf8(config_bytes).unwrap_or("grayscale");

    // Set the global static mode
    MODE = match config_str {
        "sepia" => FilterMode::Sepia,
        "red" => FilterMode::RedChannel,
        _ => FilterMode::Grayscale,
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_caps(caps_ptr: *const u8, caps_len: u32) {
    let json_bytes = slice::from_raw_parts(caps_ptr, caps_len as usize);
    let json_str = match str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(_) => return,
    };

    let caps: Caps = match serde_json::from_str(json_str) {
        Ok(c) => c,
        Err(_) => return,
    };

    CAPS = Some(caps);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn process_frame(
    in_ptr: *mut u8,
    in_len: u32,
    out_ptr: *mut u8,
    out_len: u32,
) -> i32 {
    let indata = unsafe { slice::from_raw_parts(in_ptr, in_len as usize) };
    let mut outdata = unsafe { slice::from_raw_parts_mut(out_ptr, out_len as usize) };

    let (w, h, stride) = unsafe {
        match &*core::ptr::addr_of!(CAPS) {
            Some(c) => (
                c.width,
                c.height,
                match c.format.as_str() {
                    "RGB" => 3,
                    "BGR" => 3,
                    "RGBA" => 4,
                    "BGRA" => 4,
                    _ => return -1,
                },
            ),
            None => return -1,
        }
    };

    unsafe {
        match *core::ptr::addr_of!(MODE) {
            FilterMode::Grayscale => apply_grayscale(indata, outdata, w, h, stride),
            FilterMode::Sepia => apply_sepia(indata, outdata, w, h, stride),
            FilterMode::RedChannel => apply_red_channel(indata, outdata, w, h, stride),
        }
    };
    return 0i32;
}

fn apply_grayscale(indata: &[u8], outdata: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * stride) as usize;
            let b = indata[offset] as f32;
            let g = indata[offset + 1] as f32;
            let r = indata[offset + 2] as f32;
            let gray = (0.114 * b + 0.587 * g + 0.299 * r) as u8;
            outdata[offset] = gray;
            outdata[offset + 1] = gray;
            outdata[offset + 2] = gray;
        }
    }
}

fn apply_sepia(indata: &[u8], outdata: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * stride) as usize;
            let b = indata[offset] as f32;
            let g = indata[offset + 1] as f32;
            let r = indata[offset + 2] as f32;

            let tr = 0.393 * r + 0.769 * g + 0.189 * b;
            let tg = 0.349 * r + 0.686 * g + 0.168 * b;
            let tb = 0.272 * r + 0.534 * g + 0.131 * b;

            outdata[offset] = tb.min(255.0) as u8;
            outdata[offset + 1] = tg.min(255.0) as u8;
            outdata[offset + 2] = tr.min(255.0) as u8;
        }
    }
}

fn apply_red_channel(indata: &[u8], outdata: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * stride) as usize;
            outdata[offset] = 0; // B
            outdata[offset + 1] = 0; // G
            outdata[offset + 2] = indata[offset + 2]; // R
        }
    }
}
