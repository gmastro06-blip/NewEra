/// ndi_receiver.rs — Thread dedicado que consume frames NDI.
///
/// Carga el NDI Runtime mediante libloading: no requiere NDI SDK instalado.
///
/// En Windows busca Processing.NDI.Lib.x64.dll en rutas conocidas.
/// En Linux  busca libndi.so.6 / libndi.so en rutas conocidas.
///
/// Comportamiento:
/// - Busca y carga la DLL/SO al iniciar. Si no la encuentra, retry cada N seg.
/// - Busca la fuente configurada en la LAN. Retry hasta encontrarla.
/// - Captura frames ~30 FPS y los publica en FrameBuffer.
/// - Si la fuente desaparece, vuelve a buscar. Nunca crashea.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_float, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, error, info, warn};

use crate::config::NdiConfig;
use crate::sense::frame_buffer::{Frame, FrameBuffer};

// ── Rutas candidatas del NDI Runtime ─────────────────────────────────────────

#[cfg(target_os = "windows")]
const NDI_LIB_CANDIDATES: &[&str] = &[
    r"C:\Program Files\NDI\NDI 6 Runtime\v6\Processing.NDI.Lib.x64.dll",
    r"C:\Program Files\NDI\NDI 6 Tools\Runtime\Processing.NDI.Lib.x64.dll",
    r"C:\Program Files\NDI\NDI 6 Runtime\Processing.NDI.Lib.x64.dll",
    r"C:\Program Files\NDI\NDI 5 Runtime\Processing.NDI.Lib.x64.dll",
    r"C:\ProgramData\obs-studio\plugins\distroav\bin\64bit\Processing.NDI.Lib.x64.dll",
    r"C:\Program Files\obs-studio\obs-plugins\64bit\Processing.NDI.Lib.x64.dll",
];

#[cfg(target_os = "linux")]
const NDI_LIB_CANDIDATES: &[&str] = &[
    "/usr/lib/libndi.so.6",
    "/usr/lib/libndi.so.5",
    "/usr/lib/libndi.so",
    "/usr/local/lib/libndi.so.6",
    "/usr/local/lib/libndi.so",
    // NDI SDK for Linux instala en /usr/lib por defecto
];

// ── ABI del NDI Runtime (estable entre versiones) ─────────────────────────────

#[repr(C)]
struct NdiFindCreateT {
    show_local_sources: bool,
    groups:             *const c_char,
    extra_ips:          *const c_char,
}

#[repr(C)]
#[derive(Clone)]
struct NdiSourceT {
    name:        *const c_char,
    url_address: *const c_char,
}

#[repr(C)]
struct NdiRecvCreateT {
    source_to_connect_to: NdiSourceT,
    color_format:         c_uint, // BGRX_BGRA = 1
    bandwidth:            c_int,  // highest = 100
    allow_video_fields:   bool,
    recv_name:            *const c_char,
}

#[repr(C)]
struct NdiVideoFrameT {
    xres:                 c_int,
    yres:                 c_int,
    four_cc:              c_uint,
    frame_rate_n:         c_int,
    frame_rate_d:         c_int,
    picture_aspect_ratio: c_float,
    frame_format_type:    c_int,
    timecode:             i64,
    p_data:               *mut u8,
    line_stride_in_bytes: c_int,
    p_metadata:           *const c_char,
    timestamp:            i64,
}

const FRAME_TYPE_NONE:  c_int = 0;
const FRAME_TYPE_VIDEO: c_int = 1;
const FRAME_TYPE_ERROR: c_int = -1;

// ── Punteros de función ───────────────────────────────────────────────────────

type FnInitialize     = unsafe extern "C" fn() -> bool;
type FnDestroy        = unsafe extern "C" fn();
type FnFindCreate     = unsafe extern "C" fn(*const NdiFindCreateT) -> *mut c_void;
type FnFindGetSources = unsafe extern "C" fn(*mut c_void, *mut c_uint, u32) -> *const NdiSourceT;
type FnFindDestroy    = unsafe extern "C" fn(*mut c_void);
type FnRecvCreate     = unsafe extern "C" fn(*const NdiRecvCreateT) -> *mut c_void;
type FnRecvCapture    = unsafe extern "C" fn(*mut c_void, *mut NdiVideoFrameT, *mut c_void, *mut c_void, u32) -> c_int;
type FnRecvFreeVideo  = unsafe extern "C" fn(*mut c_void, *mut NdiVideoFrameT);
type FnRecvDestroy    = unsafe extern "C" fn(*mut c_void);

// ── Wrapper de la biblioteca ──────────────────────────────────────────────────

struct NdiLib {
    // _lib debe vivir mientras usemos los punteros de función
    _lib:          libloading::Library,
    destroy:       FnDestroy,
    find_create:   FnFindCreate,
    find_sources:  FnFindGetSources,
    find_destroy:  FnFindDestroy,
    recv_create:   FnRecvCreate,
    recv_capture:  FnRecvCapture,
    recv_free_vid: FnRecvFreeVideo,
    recv_destroy:  FnRecvDestroy,
}

impl NdiLib {
    fn load(path: &str) -> Result<Self> {
        let lib = unsafe { libloading::Library::new(path) }
            .with_context(|| format!("No se pudo cargar {path}"))?;

        macro_rules! sym {
            ($name:expr) => {{
                let s: libloading::Symbol<*const ()> = unsafe { lib.get($name) }
                    .with_context(|| {
                        format!("Simbolo no encontrado: {}", String::from_utf8_lossy($name))
                    })?;
                unsafe { std::mem::transmute_copy(&*s) }
            }};
        }

        let init: FnInitialize = sym!(b"NDIlib_initialize\0");
        if !unsafe { init() } {
            bail!("NDIlib_initialize() retorno false");
        }

        Ok(NdiLib {
            destroy:       sym!(b"NDIlib_destroy\0"),
            find_create:   sym!(b"NDIlib_find_create_v2\0"),
            find_sources:  sym!(b"NDIlib_find_get_current_sources\0"),
            find_destroy:  sym!(b"NDIlib_find_destroy\0"),
            recv_create:   sym!(b"NDIlib_recv_create_v3\0"),
            recv_capture:  sym!(b"NDIlib_recv_capture_v2\0"),
            recv_free_vid: sym!(b"NDIlib_recv_free_video_v2\0"),
            recv_destroy:  sym!(b"NDIlib_recv_destroy\0"),
            _lib: lib,
        })
    }
}

impl Drop for NdiLib {
    fn drop(&mut self) {
        unsafe { (self.destroy)() };
    }
}

// ── Entry point público ───────────────────────────────────────────────────────

/// Lanza el thread NDI receiver. Se auto-recupera ante cualquier error.
pub fn spawn(config: NdiConfig, buffer: Arc<FrameBuffer>) {
    std::thread::Builder::new()
        .name("ndi-receiver".into())
        .spawn(move || {
            info!("NDI receiver thread arrancando");
            let retry = Duration::from_secs_f64(config.retry_interval_secs);
            loop {
                match run_session(&config, &buffer) {
                    Ok(()) => warn!("NDI source desconectada, reintentando..."),
                    Err(e) => error!("NDI receiver error: {:#}. Reintentando en {}s",
                                     e, config.retry_interval_secs),
                }
                std::thread::sleep(retry);
            }
        })
        .expect("No se pudo lanzar el thread ndi-receiver");
}

// ── Sesion completa: cargar lib → buscar fuente → capturar ───────────────────

fn run_session(config: &NdiConfig, buffer: &FrameBuffer) -> Result<()> {
    // 1. Localizar y cargar la biblioteca NDI
    let lib_path = find_ndi_lib(config)?;
    let lib_path_str = lib_path.to_str()
        .with_context(|| format!("Ruta NDI no es UTF-8 válido: {}", lib_path.display()))?;
    let ndi = NdiLib::load(lib_path_str)?;
    info!("NDI Runtime cargado desde {}", lib_path.display());

    // 2. Buscar la fuente (bloquea hasta encontrarla)
    let source_name = find_source(&ndi, config)?;
    info!("NDI source encontrada: {}", source_name);

    // 3. Conectar y capturar frames
    capture_loop(&ndi, &source_name, config, buffer)
}

// ── 1. Localizar la biblioteca ────────────────────────────────────────────────

fn find_ndi_lib(config: &NdiConfig) -> Result<PathBuf> {
    // Variable de entorno tiene prioridad
    if let Ok(dir) = std::env::var("NDI_RUNTIME_DIR") {
        #[cfg(target_os = "windows")]
        let name = "Processing.NDI.Lib.x64.dll";
        #[cfg(not(target_os = "windows"))]
        let name = "libndi.so.6";

        let p = PathBuf::from(&dir).join(name);
        if p.exists() {
            return Ok(p);
        }
        warn!("NDI_RUNTIME_DIR={dir} pero no se encontro {name} ahi");
    }

    // Buscar en candidatos conocidos
    for &candidate in NDI_LIB_CANDIDATES {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }

    let retry = Duration::from_secs_f64(config.retry_interval_secs);
    warn!(
        "NDI Runtime no encontrado. Candidatos revisados:\n{}\nSetea NDI_RUNTIME_DIR o instala NDI Tools. Reintentando en {}s...",
        NDI_LIB_CANDIDATES.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n"),
        config.retry_interval_secs
    );
    std::thread::sleep(retry);

    Err(anyhow!("NDI Runtime no encontrado"))
}

// ── 2. Buscar la fuente NDI ───────────────────────────────────────────────────

fn find_source(ndi: &NdiLib, config: &NdiConfig) -> Result<String> {
    let retry = Duration::from_secs_f64(config.retry_interval_secs);

    loop {
        let cfg = NdiFindCreateT {
            show_local_sources: true,
            groups:             std::ptr::null(),
            extra_ips:          std::ptr::null(),
        };

        let finder = unsafe { (ndi.find_create)(&cfg) };
        if finder.is_null() {
            bail!("NDIlib_find_create_v2 retorno null");
        }

        // 1s para recibir broadcasts NDI de la LAN
        std::thread::sleep(Duration::from_secs(1));

        let mut count: c_uint = 0;
        let raw = unsafe { (ndi.find_sources)(finder, &mut count, 1000) };

        let mut found: Option<String> = None;

        if !raw.is_null() && count > 0 {
            for i in 0..count as usize {
                let src  = unsafe { &*raw.add(i) };
                if src.name.is_null() { continue; }
                let name = unsafe { CStr::from_ptr(src.name) }
                    .to_string_lossy()
                    .to_string();

                if name.contains(config.source_name.as_str()) {
                    found = Some(name);
                    break;
                }
            }
        }

        unsafe { (ndi.find_destroy)(finder) };

        if let Some(name) = found {
            return Ok(name);
        }

        warn!(
            "NDI: fuente '{}' no encontrada ({} visibles). Reintentando en {}s...",
            config.source_name, count, config.retry_interval_secs
        );
        std::thread::sleep(retry);
    }
}

// ── 3. Loop de captura ────────────────────────────────────────────────────────

fn capture_loop(
    ndi:    &NdiLib,
    source: &str,
    _config: &NdiConfig,
    buffer: &FrameBuffer,
) -> Result<()> {
    let name_c = CString::new(source)
        .with_context(|| format!("Nombre de fuente NDI contiene null bytes: {source}"))?;
    let recv_name = CString::new("tibia-bot").expect("literal sin null bytes");

    let src_desc = NdiSourceT {
        name:        name_c.as_ptr(),
        url_address: std::ptr::null(),
    };

    let recv_cfg = NdiRecvCreateT {
        source_to_connect_to: src_desc,
        color_format:         0,   // BGRX_BGRA (0=siempre 4 bytes/pixel)
        bandwidth:            100, // highest
        allow_video_fields:   false,
        recv_name:            recv_name.as_ptr(),
    };

    let recv = unsafe { (ndi.recv_create)(&recv_cfg) };
    if recv.is_null() {
        bail!("NDIlib_recv_create_v3 retorno null para '{source}'");
    }

    info!("NDI receiver conectado a '{source}'");

    debug!("capture_loop: recv handle = {:?}, iniciando loop", recv);

    loop {
        let t0 = Instant::now();
        let mut video = std::mem::MaybeUninit::<NdiVideoFrameT>::zeroed();

        debug!("capture_loop: llamando recv_capture...");
        let frame_type = unsafe {
            (ndi.recv_capture)(
                recv,
                video.as_mut_ptr(),
                std::ptr::null_mut(), // audio: ignorar
                std::ptr::null_mut(), // metadata: ignorar
                200,                  // timeout ms (>33ms/frame@30fps)
            )
        };
        debug!("capture_loop: recv_capture retornó {}", frame_type);

        match frame_type {
            FRAME_TYPE_VIDEO => {
                let v = unsafe { video.assume_init() };

                let width  = v.xres as u32;
                let height = v.yres as u32;
                debug!("NDI video frame: {}x{}  fourcc={:#010x}  stride={}  p_data={:?}",
                       width, height, v.four_cc, v.line_stride_in_bytes, v.p_data);

                // Sanity check antes de acceder a la memoria del frame.
                // Si las dimensiones son implausibles el struct layout no coincide.
                let raw_data = if width == 0 || height == 0 || width > 8192 || height > 8192 {
                    warn!("NDI frame con dimensiones implausibles {}x{} — descartando", width, height);
                    vec![]
                } else {
                    // Usar el stride que NDI reporta (puede haber padding).
                    // NO forzar width*4 — el fourcc determina bytes/pixel.
                    let stride   = v.line_stride_in_bytes as usize;
                    let byte_len = stride * height as usize;
                    if v.p_data.is_null() || byte_len == 0 {
                        vec![]
                    } else {
                        unsafe { std::slice::from_raw_parts(v.p_data, byte_len) }.to_vec()
                    }
                };

                // Liberar el buffer NDI antes de soltar el puntero
                let mut v_free = v;
                unsafe { (ndi.recv_free_vid)(recv, &mut v_free) };

                let frame = Frame {
                    width,
                    height,
                    data:        raw_data,
                    captured_at: t0,
                };

                buffer.publish(frame);

                debug!(
                    "NDI frame: {}x{}  captura={:.1}ms",
                    width, height,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }

            FRAME_TYPE_NONE => {
                // Timeout normal — fuente viva pero sin frame nuevo. Continuar.
            }

            FRAME_TYPE_ERROR => {
                // Fuente desconectada o error de red.
                unsafe { (ndi.recv_destroy)(recv) };
                return Ok(()); // el caller reintentará
            }

            _ => {
                // Audio, metadata, status change — ignorar silenciosamente.
            }
        }
    }
}
