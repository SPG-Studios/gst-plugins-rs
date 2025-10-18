use std::alloc::{Layout, alloc, dealloc};
use std::slice;

#[derive(Clone, Copy)]
enum FilterMode {
    Grayscale,
    Sepia,
    RedChannel,
}

static mut MODE: FilterMode = FilterMode::Grayscale;
// 1. Implement `allocate`
#[unsafe(no_mangle)]
pub extern "C" fn allocate(size: u32) -> *mut u8 {
    let layout = Layout::from_size_align(size as usize, 1).unwrap();
    unsafe { alloc(layout) }
}

// 2. Implement `deallocate`
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

// 3. Implement the main processing function
#[unsafe(no_mangle)]
pub unsafe extern "C" fn process_frame(buffer_ptr: *mut u8, width: u32, height: u32, stride: u32) {
    let size = (stride * height) as usize;
    let data = unsafe { slice::from_raw_parts_mut(buffer_ptr, size) };

    match MODE {
        FilterMode::Grayscale => apply_grayscale(data, width, height, stride),
        FilterMode::Sepia => apply_sepia(data, width, height, stride),
        FilterMode::RedChannel => apply_red_channel(data, width, height, stride),
    }
}

fn apply_grayscale(data: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            let b = data[offset] as f32;
            let g = data[offset + 1] as f32;
            let r = data[offset + 2] as f32;
            let gray = (0.114 * b + 0.587 * g + 0.299 * r) as u8;
            data[offset] = gray;
            data[offset + 1] = gray;
            data[offset + 2] = gray;
        }
    }
}

fn apply_sepia(data: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            let b = data[offset] as f32;
            let g = data[offset + 1] as f32;
            let r = data[offset + 2] as f32;

            let tr = 0.393 * r + 0.769 * g + 0.189 * b;
            let tg = 0.349 * r + 0.686 * g + 0.168 * b;
            let tb = 0.272 * r + 0.534 * g + 0.131 * b;

            data[offset] = tb.min(255.0) as u8;
            data[offset + 1] = tg.min(255.0) as u8;
            data[offset + 2] = tr.min(255.0) as u8;
        }
    }
}

fn apply_red_channel(data: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            data[offset] = 0; // B
            data[offset + 1] = 0; // G
            // data[offset + 2] = R (leave as-is)
        }
    }
}
