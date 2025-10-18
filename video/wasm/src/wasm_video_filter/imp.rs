use std::sync::Mutex;

use gst::glib::{self, prelude::*, ParamFlags};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use gst_video::VideoInfo;
use once_cell::sync::Lazy;
use wasmtime::{Engine, Instance, Module, Store};

type WasmAllocate = wasmtime::TypedFunc<u32, u32>;
type WasmDeallocate = wasmtime::TypedFunc<(u32, u32), ()>;
type WasmProcessFrame = wasmtime::TypedFunc<(u32, u32, u32, u32), ()>;
type WasmConfig = wasmtime::TypedFunc<(u32, u32), ()>;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "wasmvideofilter",
        gst::DebugColorFlags::empty(),
        Some("WASM Video Filter"),
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
            entrypoint: "process_frame".to_string(),
            config_str: None,
        }
    }
}

struct WasmState {
    store: Store<()>,
    instance: Instance,
    allocate_fn: WasmAllocate,
    deallocate_fn: WasmDeallocate,
    process_frame_fn: WasmProcessFrame,
    configure_fn: Option<WasmConfig>,
}

#[derive(Default)]
pub struct WasmVideoFilter {
    settings: Mutex<Settings>,
    wasm_state: Mutex<Option<WasmState>>,
    video_info: Mutex<Option<VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for WasmVideoFilter {
    const NAME: &'static str = "GstWasmVideoFilter";

    type Type = super::WasmVideoFilter;

    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for WasmVideoFilter {
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
                    .blurb("Name of the exported WASM function to call per frame")
                    .default_value("process_frame")
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

impl GstObjectImpl for WasmVideoFilter {}

impl ElementImpl for WasmVideoFilter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WASM Video Filter",
                "Filter/Video",
                "Executes a WebAssembly module on video frames",
                "David Maseda Neira  <david.masedan@gmail.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::Rgb,
                    gst_video::VideoFormat::Rgba,
                    gst_video::VideoFormat::Bgr,
                    gst_video::VideoFormat::Bgra,
                ])
                .build();
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

impl BaseTransformImpl for WasmVideoFilter {
    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let other_caps = caps.clone();

        gst::debug!(
            CAT,
            imp = self,
            "Transforming caps {:?} in direction {:?} with filter {:?}",
            caps,
            direction,
            filter
        );

        if let Some(filter) = filter {
            Some(other_caps.intersect(filter))
        } else {
            Some(other_caps)
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
                    ["Failed to get allocation function: {}", e]
                )
            })?;

        let process_frame_fn = instance
            .get_typed_func::<(u32, u32, u32, u32), ()>(&mut store, &settings.entrypoint)
            .map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to get allocation function: {}", e]
                )
            })?;

        let configure_fn = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "configure")
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
            process_frame_fn,
            configure_fn,
        });

        Ok(())
    }

    // Called when the pipeline stops. Clean up here.
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.obj(), "Stopping");
        *self.wasm_state.lock().unwrap() = None;
        Ok(())
    }

    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;

    const PASSTHROUGH_ON_SAME_CAPS: bool = false;

    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl WasmVideoFilter {
    fn transform_frame_internal(
        &self,
        in_frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        out_frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
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
        let process_fn = &wasm_state.process_frame_fn;

        let input_data = in_frame.plane_data(0).unwrap();
        let frame_size = input_data.len() as u32;

        let wasm_ptr = allocate_fn.call(&mut *store, frame_size)?;

        memory.write(&mut *store, wasm_ptr as usize, input_data)?;

        let width = in_frame.width();
        let height = in_frame.height();
        let stride = in_frame.plane_stride()[0];
        process_fn.call(&mut *store, (wasm_ptr, width, height, stride as u32))?;

        let mut output_data_mut = out_frame.plane_data_mut(0).unwrap();
        memory.read(&mut *store, wasm_ptr as usize, &mut output_data_mut)?;

        deallocate_fn.call(store, (wasm_ptr, frame_size))?;

        Ok(gst::FlowSuccess::Ok)
    }
}
// VideoFilter implementation
impl VideoFilterImpl for WasmVideoFilter {
    fn set_info(
        &self,
        incaps: &gst::Caps,
        in_info: &VideoInfo,
        outcaps: &gst::Caps,
        out_info: &VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        gst::log!(
            CAT,
            obj = self.obj(),
            "Setting format info: caps={}",
            incaps
        );
        // Store the negotiated video info for later use.
        *self.video_info.lock().unwrap() = Some(in_info.clone());

        self.parent_set_info(incaps, in_info, outcaps, out_info)
    }

    fn transform_frame(
        &self,
        in_frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        out_frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // TODO: This is where we will call the WASM function.
        // For now, just copy input to output to have a working passthrough filter.
        in_frame.copy(out_frame).unwrap();
        match self.transform_frame_internal(in_frame, out_frame) {
            Ok(ret) => Ok(ret),
            Err(e) => {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Failed,
                    ("Failed to process frame in WASM runtime: {}", e)
                );
                Err(gst::FlowError::Error)
            }
        }
    }
}
