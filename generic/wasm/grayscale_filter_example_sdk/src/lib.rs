use gst_wasm_filter_sdk::define_wasm_module;
use gst_wasm_filter_sdk::{Caps, WasmModuleImpl};

#[derive(Clone, Copy)]
enum FilterMode {
    Grayscale,
    Sepia,
    RedChannel,
}

struct ColorFilter {
    width: u32,
    height: u32,
    bpp: u8,
    rgb_positions: [usize; 3],
    mode: FilterMode,
}

impl WasmModuleImpl for ColorFilter {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            bpp: 0,
            mode: FilterMode::Grayscale,
            rgb_positions: [0, 0, 0],
        }
    }

    fn set_caps(&mut self, caps: &Caps) {
        self.width = caps
            .fields
            .get("width")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        self.height = caps
            .fields
            .get("height")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        self.bpp = match caps.fields.get("format").and_then(|v| v.as_str()) {
            Some("RGBA") | Some("BGRA") | Some("ARGB") | Some("ABGR") => 4,
            Some("RGB") | Some("BGR") => 3,
            _ => 0, //unsupported format
        };

        self.rgb_positions = match caps.fields.get("format").and_then(|v| v.as_str()) {
            Some("RGBA") | Some("RGB") => [0, 1, 2],
            Some("BGRA") | Some("BGR") => [2, 1, 0],
            Some("ARGB") => [1, 2, 3],
            Some("ABGR") => [3, 2, 1],
            _ => return, //unsupported format
        };
    }

    fn set_config(&mut self, config: String) {
        self.mode = match config.as_str() {
            "gray" | "grey" => FilterMode::Grayscale,
            "sepia" => FilterMode::Sepia,
            "red" => FilterMode::RedChannel,
            _ => FilterMode::Grayscale,
        }
    }

    fn process_buffer(&mut self, input: &[u8], output: &mut [u8]) -> i32 {
        if self.bpp < 3 || self.width == 0 || self.height == 0 {
            return -1;
        }

        let bytes_to_process = (self.width * self.height * self.bpp as u32) as usize;
        if input.len() < bytes_to_process || output.len() < bytes_to_process {
            return -2;
        }

        let processing_slice = &mut output[..bytes_to_process];

        processing_slice.copy_from_slice(&input[..bytes_to_process]);

        for pixel in processing_slice.chunks_mut(self.bpp as usize) {
            let r = pixel[self.rgb_positions[0]] as f32;
            let g = pixel[self.rgb_positions[1]] as f32;
            let b = pixel[self.rgb_positions[2]] as f32;

            let gray = (0.114 * b + 0.587 * g + 0.299 * r) as u8;
            match self.mode {
                FilterMode::Grayscale => {
                    pixel[0] = gray;
                    pixel[1] = gray;
                    pixel[2] = gray;
                }
                FilterMode::Sepia => {
                    let tr = 0.393 * r + 0.769 * g + 0.189 * b;
                    let tg = 0.349 * r + 0.686 * g + 0.168 * b;
                    let tb = 0.272 * r + 0.534 * g + 0.131 * b;
                    pixel[self.rgb_positions[0]] = tr.min(255.0) as u8;
                    pixel[self.rgb_positions[1]] = tg.min(255.0) as u8;
                    pixel[self.rgb_positions[2]] = tb.min(255.0) as u8;
                }
                FilterMode::RedChannel => {
                    pixel[self.rgb_positions[1]] = 0;
                    pixel[self.rgb_positions[2]] = 0;
                }
            }
        }

        bytes_to_process as i32
    }
}

define_wasm_module!(ColorFilter);
