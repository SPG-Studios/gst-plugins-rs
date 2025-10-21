use std::str::FromStr;
use std::sync::Mutex;

use gst::glib::{self, prelude::*, ParamFlags};
use gst::subclass::prelude::*;
use gst::{prelude::*, SerializeFlags};
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::VideoInfo;
use once_cell::sync::Lazy;
use wasmtime::{Engine, Instance, Module, Store};

use crate::json_caps;

//ptr,len
type WasmAllocate = wasmtime::TypedFunc<u32, u32>;
//ptr,len
type WasmDeallocate = wasmtime::TypedFunc<(u32, u32), ()>;
//in_ptr,in_len,out_ptr,out_len -> result
type WasmProcessBuffer = wasmtime::TypedFunc<(u32, u32, u32, u32), i32>;
//caps_str,caps_len
type WasmSetCaps = wasmtime::TypedFunc<(u32, u32), ()>;
//caps_str,caps_len -> (out_caps_ptr << 32 | out_caps_len)
type WasmTransformCaps = wasmtime::TypedFunc<(u32, u32), i64>;
//ptr
type WasmFreeResult = wasmtime::TypedFunc<u32, ()>;
//config_ptr,config_len
type WasmConfig = wasmtime::TypedFunc<(u32, u32), ()>;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "wasmfilter",
        gst::DebugColorFlags::empty(),
        Some("WASM Filter"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    module_path: Option<String>,
    entrypoint: String,
    config_str: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            module_path: None,
            entrypoint: "process_buffer".to_string(),
            config_str: None,
        }
    }
}

struct WasmState {
    store: Store<()>,
    instance: Instance,
    allocate_fn: WasmAllocate,
    deallocate_fn: WasmDeallocate,
    process_buffer_fn: WasmProcessBuffer,
    configure_fn: Option<WasmConfig>,
    set_caps_fn: Option<WasmSetCaps>,
    transform_caps_fn: Option<WasmTransformCaps>,
    free_result_fn: Option<WasmFreeResult>,
}

#[derive(Default)]
pub struct WasmFilter {
    settings: Mutex<Settings>,
    wasm_state: Mutex<Option<WasmState>>,
}

#[glib::object_subclass]
impl ObjectSubclass for WasmFilter {
    const NAME: &'static str = "GstWasmFilter";

    type Type = super::WasmFilter;

    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for WasmFilter {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("module")
                    .nick("Module")
                    .blurb("path to .wasm module")
                    .flags(ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("entrypoint")
                    .nick("Entrypoint")
                    .blurb("Name of the exported WASM function to call per buffer")
                    .default_value("process_buffer")
                    .flags(ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("config-str")
                    .nick("Config String")
                    .blurb("Config string being passed into the WASM module")
                    .flags(ParamFlags::READWRITE)
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "module" => {
                settings.module_path = value.get().expect("type checked by GObject");
            }
            "entrypoint" => {
                settings.entrypoint = value.get().expect("type checked by GObject");
            }
            "config-str" => {
                settings.config_str = value.get().expect("type checked by GObject");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "module" => settings.module_path.to_value(),
            "entrypoint" => settings.entrypoint.to_value(),
            "config-str" => settings.config_str.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for WasmFilter {}

impl ElementImpl for WasmFilter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WASM Filter",
                "Filter",
                "Executes a WebAssembly module on GStreamer buffers",
                "David Maseda Neira  <david.masedan@gmail.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
            ]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for WasmFilter {
    const MODE: gst_base::subclass::BaseTransformMode = gst_base::subclass::BaseTransformMode::Both;

    const PASSTHROUGH_ON_SAME_CAPS: bool = false;

    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut wasm_state_guard = self.wasm_state.lock().unwrap();
        let wasm_state = match wasm_state_guard.as_mut() {
            Some(s) => s,
            None => {
                // WASM module not loaded yet, use passthrough
                return Some(
                    caps.intersect_with_mode(filter.unwrap_or(caps), gst::CapsIntersectMode::First),
                );
            }
        };

        // Check if the WASM module supports transforming caps.
        // If not, we just propose intersection with the filter caps

        let (transform_caps_fn, free_result_fn) = match (
            wasm_state.transform_caps_fn.clone(),
            wasm_state.free_result_fn.clone(),
        ) {
            (Some(t), Some(f)) => (t, f),
            _ => {
                gst::debug!(CAT,obj = self.obj(), "WASM module does not export 'transform_caps' and 'free_result'. Using passthrough logic");
                return Some(
                    caps.intersect_with_mode(filter.unwrap_or(caps), gst::CapsIntersectMode::First),
                );
            }
        };

        let (in_ptr, in_len) = match self.copy_string_to_wasm(&mut *wasm_state, &caps.to_string()) {
            Ok(val) => val,
            Err(e) => {
                gst::error!(CAT, obj = self.obj(), "Failed to copy caps to WASM: {}", e);
                return None;
            }
        };

        // Call the actual WASM function to transform caps
        let result_i64 = match transform_caps_fn.call(&mut wasm_state.store, (in_ptr, in_len)) {
            Ok(res) => res,
            Err(e) => {
                gst::error!(
                    CAT,
                    obj = self.obj(),
                    "WASM 'transform_caps' trapped: {}",
                    e
                );
                return None;
            }
        };

        self.deallocate_in_wasm(&mut *wasm_state, in_ptr, in_len);

        if result_i64 == 0 {
            return Some(
                caps.intersect_with_mode(filter.unwrap_or(caps), gst::CapsIntersectMode::First),
            );
        }

        // Unpack ptr and len from i64 return value
        let out_ptr = (result_i64 >> 32) as u32;
        let out_len = result_i64 as u32;

        let result_str = match self.read_string_from_wasm(wasm_state, out_ptr, out_len) {
            Ok(s) => s,
            Err(e) => {
                gst::error!(
                    CAT,
                    obj = self.obj(),
                    "Failed to read result from WASM: {}",
                    e
                );
                return None;
            }
        };

        if let Err(e) = free_result_fn.call(&mut wasm_state.store, out_ptr) {
            gst::warning!(CAT, obj = self.obj(), "WASM 'free_result' trapped: {}", e);
        }

        let result_caps = gst::Caps::from_str(&result_str).unwrap();

        if let Some(f) = filter {
            Some(result_caps.intersect_with_mode(f, gst::CapsIntersectMode::First))
        } else {
            Some(result_caps)
        }
    }

    fn transform_size(
        &self,
        direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        // For now, assume output size equals input size
        Some(size)
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let mut wasm_state_guard = self.wasm_state.lock().unwrap();
        let wasm_state = match wasm_state_guard.as_mut() {
            Some(s) => s,
            None => return Ok(()),
        };

        //Inform WASM about the negotiated caps

        let json_caps = json_caps::caps_to_json_string(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "Failed to convert caps to json: {}", e))?;

        if let Some(set_caps_fn) = wasm_state.set_caps_fn.clone() {
            let (ptr, len) = self
                .copy_string_to_wasm(wasm_state, &json_caps)
                .map_err(|e| gst::loggable_error!(CAT, "Failed to copy caps: {}", e))?;

            if let Err(e) = set_caps_fn.call(&mut wasm_state.store, (ptr, len)) {
                gst::warning!(CAT, obj = self.obj(), "WASM 'set_caps' trapped: {}", e);
            }
            self.deallocate_in_wasm(wasm_state, ptr, len);
        }
        Ok(())
    }

    fn transform(
        &self,
        in_buf: &gst::Buffer,
        out_buf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        match self.transform_buffer_internal(in_buf, out_buf) {
            Ok(s) => {
                // Copy metadata (timestamps, duration, flags) from input to output
                let _ = in_buf.copy_into(
                    out_buf,
                    gst::BufferCopyFlags::TIMESTAMPS
                        | gst::BufferCopyFlags::FLAGS
                        | gst::BufferCopyFlags::META,
                    ..,
                );
                Ok(s)
            }
            Err(e) => {
                gst::error!(CAT, obj = self.obj(), "Trnasform buffer failed: {}", e);
                Err(gst::FlowError::Error)
            }
        }
    }
    // Called when the pipeline goes to PLAYING. Load the WASM module here.
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.obj(), "Starting");
        let settings = self.settings.lock().unwrap().clone();

        let module_path = match settings.module_path {
            Some(path) => path,
            None => {
                return Err(gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["WASM module path ('module' property) not set."]
                ));
            }
        };

        // Initialize wasmtime
        let engine = Engine::default();
        let module = Module::from_file(&engine, &module_path).map_err(|e| {
            gst::error_msg!(
                gst::ResourceError::Read,
                ["Failed to load WASM module from '{}': {}", module_path, e]
            )
        })?;

        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).map_err(|e| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to instantiate WASM module: {}", e]
            )
        })?;

        let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to get handler for wasm module memory"]
            )
        })?;

        let allocate_fn = instance
            .get_typed_func::<u32, u32>(&mut store, "allocate")
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to get allocation function: {}", e]
                )
            })?;
        let deallocate_fn = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "deallocate")
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to get deallocation function: {}", e]
                )
            })?;

        let process_buffer_fn = instance
            .get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, &settings.entrypoint)
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to get process_buffer function: {}", e]
                )
            })?;

        let set_caps_fn = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "set_caps")
            .ok();

        let transform_caps_fn = instance
            .get_typed_func::<(u32, u32), i64>(&mut store, "transform_caps")
            .ok();

        let free_result_fn = instance
            .get_typed_func::<u32, ()>(&mut store, "free_result")
            .ok();

        let configure_fn = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "set_config")
            .ok();
        if let Some(config_str) = settings.config_str {
            let config_str_len = config_str.len();
            match &configure_fn {
                Some(f) => {
                    let wasm_ptr = allocate_fn
                        .call(&mut store, config_str_len as u32)
                        .map_err(|e| {
                            gst::error_msg!(
                                gst::CoreError::Failed,
                                ["Failed to allocate memory for the config string: {}", e]
                            )
                        })?;

                    memory
                        .write(&mut store, wasm_ptr as usize, &config_str.into_bytes())
                        .map_err(|e| {
                            gst::error_msg!(
                                gst::CoreError::Failed,
                                ["Failed to copy config string to WASM memory: {}", e]
                            )
                        })?;

                    f.call(&mut store, (wasm_ptr, config_str_len as u32))
                        .map_err(|e| {
                            gst::error_msg!(
                                gst::CoreError::Failed,
                                [
                                    "Failed to call the configure function on WASM module: {}",
                                    e
                                ]
                            )
                        })?;

                    deallocate_fn
                        .call(&mut store, (wasm_ptr, config_str_len as u32))
                        .map_err(|e| {
                            gst::error_msg!(
                                gst::CoreError::Failed,
                                ["Failed to deallocate memory: {}", e]
                            )
                        })?;
                }
                None => {
                    gst::warning!(
                        CAT,
                        obj = self.obj(),
                        "Config string defined, but WASM module does not expose a configure function"
                    );
                }
            }
        }

        *self.wasm_state.lock().unwrap() = Some(WasmState {
            store,
            instance,
            allocate_fn,
            deallocate_fn,
            process_buffer_fn,
            configure_fn,
            set_caps_fn,
            transform_caps_fn,
            free_result_fn,
        });

        Ok(())
    }

    // Called when the pipeline stops. Clean up here.
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.obj(), "Stopping");
        *self.wasm_state.lock().unwrap() = None;
        Ok(())
    }
}

impl WasmFilter {
    fn transform_buffer_internal(
        &self,
        in_buffer: &gst::Buffer,
        out_buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, anyhow::Error> {
        let mut wasm_state_guard = self.wasm_state.lock().unwrap();
        let wasm_state = wasm_state_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("WASM state not initialized"))?;

        let store = &mut wasm_state.store;
        let instance = &wasm_state.instance;
        let settings = self.settings.lock().unwrap();

        let memory = instance
            .get_memory(&mut *store, "memory")
            .ok_or_else(|| anyhow::anyhow!("WASM module must export 'memory'"))?;
        let allocate_fn = &wasm_state.allocate_fn;
        let deallocate_fn = &wasm_state.deallocate_fn;
        let process_buffer_fn = &wasm_state.process_buffer_fn;

        let in_map = in_buffer.map_readable()?;
        let input_data = in_map.as_slice();
        let input_size = input_data.len() as u32;
        let input_ptr = allocate_fn.call(&mut *store, input_size)?;

        //let out_buffer_ref = out_buffer.make_mut();
        let mut out_map = out_buffer.map_writable()?;
        let mut output_data = out_map.as_mut_slice();
        let output_size = output_data.len() as u32;
        let output_ptr = allocate_fn.call(&mut *store, output_size)?;

        memory.write(&mut *store, input_ptr as usize, input_data)?;

        let result: i32 = process_buffer_fn.call(
            &mut *store,
            (input_ptr, input_size, output_ptr, output_size),
        )?;

        memory.read(&mut *store, output_ptr as usize, &mut output_data)?;

        deallocate_fn.call(&mut *store, (input_ptr, input_size))?;
        deallocate_fn.call(store, (output_ptr, output_size))?;

        Ok(gst::FlowSuccess::Ok)
    }

    fn copy_string_to_wasm(
        &self,
        wasm_state: &mut WasmState,
        s: &str,
    ) -> Result<(u32, u32), anyhow::Error> {
        let bytes = s.as_bytes();
        let len = bytes.len() as u32;
        let ptr = wasm_state.allocate_fn.call(&mut wasm_state.store, len)?;

        let memory = wasm_state
            .instance
            .get_memory(&mut wasm_state.store, "memory")
            .unwrap();
        memory.write(&mut wasm_state.store, ptr as usize, bytes)?;

        Ok((ptr, len))
    }

    fn read_string_from_wasm(
        &self,
        wasm_state: &mut WasmState,
        ptr: u32,
        len: u32,
    ) -> Result<String, anyhow::Error> {
        let memory = wasm_state
            .instance
            .get_memory(&mut wasm_state.store, "memory")
            .unwrap();
        let mut buffer = vec![0; len as usize];
        memory.read(&mut wasm_state.store, ptr as usize, &mut buffer);
        Ok(String::from_utf8(buffer)?)
    }

    fn deallocate_in_wasm(&self, wasm_state: &mut WasmState, ptr: u32, len: u32) {
        if let Err(e) = wasm_state
            .deallocate_fn
            .call(&mut wasm_state.store, (ptr, len))
        {
            gst::warning!(CAT, obj = self.obj(), "WASM 'deallocate' trapped: {}", e);
        }
    }
}
