// ============================================================================
// 🚀 LITECORD GPU HARDWARE ACCELERATED VIDEO ENCODER ENGINE (SUNSHINE-GRADE)
// ============================================================================
// Provedor unificado de codificação de vídeo com suporte a:
// 1. NVIDIA NVENC (GeForce / Quadro Hardware Acceleration via nvEncodeAPI64 / FFmpeg)
// 2. AMD AMF (Radeon Hardware Acceleration via amfrt64 Zero-Copy)
// 3. Windows Media Foundation D3D11 (Universal Windows GPU Acceleration)
// 4. OpenH264 SIMD Multi-threading com Rayon Paralelizado (Fallback Universal CPU)
// 5. Bitrate Dinâmico Adaptativo (Sunshine/WebRTC AIMD Rate Control)
// ============================================================================
#![allow(dead_code, non_snake_case, non_camel_case_types)]

pub mod nvenc;
pub use nvenc as ffmpeg_nvenc;

#[cfg(target_os = "windows")]
pub mod amd_amf;

#[cfg(target_os = "windows")]
pub mod wmf;

#[cfg(target_os = "windows")]
pub mod nvenc_api;
#[cfg(target_os = "windows")]
pub mod nvenc_d3d11;

pub mod software;
pub mod h264_vui;

#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod vaapi;

pub use nvenc::FfmpegNvencEncoder;
#[cfg(target_os = "windows")]
pub use amd_amf::AmdAmfZeroCopyEncoder;
#[cfg(target_os = "windows")]
pub use wmf::WmfGpuEncoder;
pub use software::OpenH264Encoder;

use log::{info, warn};

/// Interface unificada para qualquer engine de codificação de vídeo
pub trait VideoEncoder: Send {
    /// Codifica um frame BGRA para stream H.264 NAL units
    fn encode(&mut self, bgra_data: &[u8], width: u32, height: u32) -> Option<Vec<u8>>;

    /// Força a geração imediata de um IDR / Keyframe (PLI Recovery)
    fn force_intra_frame(&mut self);

    /// Ajusta dinamicamente a taxa de bits (Adaptive Bitrate)
    fn set_bitrate_bps(&mut self, bitrate_bps: u32);

    /// Retorna a taxa de bits atual configurada em bps
    fn get_bitrate_bps(&self) -> u32;

    /// Asks for H.264 Main (`true`: every viewer decodes it) or Constrained Baseline, called
    /// before each `encode`. Encoders with a single profile ignore it. Going to Main may wait
    /// for the next keyframe the caller asks for; with `false`, the next picture out is
    /// Constrained Baseline again.
    fn set_efficient_profile(&mut self, _on: bool) {}

    /// The quality floor (lowest QP) the stream controls ask for, called before each
    /// `encode`. Only the VA-API encoder has one; it replaces its pipeline for a new value,
    /// one replacement at a time, and goes back to the previous floor if the new one fails.
    /// 0 asks for nothing (the share has ended).
    fn set_quality_floor(&mut self, _min_qp: u32) {}

    /// Retorna o nome amigável do backend ativo
    fn name(&self) -> &'static str;

    /// Indica se está rodando em hardware dedicado de GPU
    fn is_hardware_accelerated(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuHardwareType {
    NvidiaNvenc,
    AmdAmf,
    SoftwareOnly,
}

pub fn detect_gpu_hardware() -> GpuHardwareType {
    #[cfg(target_os = "windows")]
    unsafe {
        let nvenc_dll = windows_sys::Win32::System::LibraryLoader::LoadLibraryA(b"nvEncodeAPI64.dll\0".as_ptr());
        if !nvenc_dll.is_null() {
            let get_proc = windows_sys::Win32::System::LibraryLoader::GetProcAddress;
            let create_instance_fn = get_proc(nvenc_dll, b"NvEncodeAPICreateInstance\0".as_ptr());
            if create_instance_fn.is_some() {
                windows_sys::Win32::Foundation::FreeLibrary(nvenc_dll);
                return GpuHardwareType::NvidiaNvenc;
            }
            windows_sys::Win32::Foundation::FreeLibrary(nvenc_dll);
        }

        let amf_dll = windows_sys::Win32::System::LibraryLoader::LoadLibraryA(b"amfrt64.dll\0".as_ptr());
        if !amf_dll.is_null() {
            let get_proc = windows_sys::Win32::System::LibraryLoader::GetProcAddress;
            let amf_query_version_fn = get_proc(amf_dll, b"AMFQueryVersion\0".as_ptr());
            if amf_query_version_fn.is_some() {
                windows_sys::Win32::Foundation::FreeLibrary(amf_dll);
                return GpuHardwareType::AmdAmf;
            }
            windows_sys::Win32::Foundation::FreeLibrary(amf_dll);
        }
    }

    #[cfg(target_os = "linux")]
    unsafe {
        for nv_name in [b"libnvidia-encode.so.1\0".as_ptr(), b"libnvidia-encode.so\0".as_ptr()] {
            let h = libc::dlopen(nv_name as *const libc::c_char, libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if !h.is_null() {
                let sym = libc::dlsym(h, b"NvEncodeAPICreateInstance\0".as_ptr() as *const libc::c_char);
                libc::dlclose(h);
                if !sym.is_null() {
                    return GpuHardwareType::NvidiaNvenc;
                }
            }
        }

        for amf_name in [b"libamfrt64.so.1\0".as_ptr(), b"libamfrt64.so\0".as_ptr()] {
            let h = libc::dlopen(amf_name as *const libc::c_char, libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if !h.is_null() {
                let sym = libc::dlsym(h, b"AMFQueryVersion\0".as_ptr() as *const libc::c_char);
                libc::dlclose(h);
                if !sym.is_null() {
                    return GpuHardwareType::AmdAmf;
                }
            }
        }
    }

    GpuHardwareType::SoftwareOnly
}

/// To be called right before `std::process::exit` (see `vaapi::before_exit`).
pub fn before_exit() {
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    vaapi::before_exit();
}

/// Cria a melhor instância de codificador de vídeo disponível para a plataforma
pub fn create_best_encoder(target_fps: u32, is_screen_content: bool) -> Box<dyn VideoEncoder> {
    info!("🔍 [VIDEO CODEC FACTORY] Avaliando melhor engine de codificação para o sistema...");

    #[cfg(target_os = "windows")]
    let selection = crate::screen_capture::gpu_tx::encoder_choice();
    #[cfg(not(target_os = "windows"))]
    let selection = crate::video_settings::get_video_encoder();
    #[cfg(target_os = "windows")]
    let manual_selection = match crate::screen_capture::gpu_capture::parse_choice(&selection) {
        crate::screen_capture::gpu_capture::Choice::Cpu => "openh264",
        crate::screen_capture::gpu_capture::Choice::Gpu { vendor: 0x10de, .. } => "nvenc",
        crate::screen_capture::gpu_capture::Choice::Gpu { vendor: 0x8086, .. } => "wmf",
        crate::screen_capture::gpu_capture::Choice::Gpu { vendor: 0x1002, .. } => "amf",
        _ => "auto",
    };
    #[cfg(not(target_os = "windows"))]
    let manual_selection = selection.as_str();
    if manual_selection != "auto" {
        info!("🎯 [VIDEO CODEC FACTORY] Codificador manual configurado pelo usuário: '{}'", manual_selection);
        match manual_selection {
            // The FFmpeg loader only knows h264_nvenc and writes AVCodecContext fields at
            // fixed offsets: on Linux it is not loaded without NVENC on the machine.
            #[cfg(target_os = "linux")]
            "nvenc" | "ffmpeg" if detect_gpu_hardware() != GpuHardwareType::NvidiaNvenc => {
                warn!("⚠️ [VIDEO CODEC FACTORY] NVENC manual: sem NVENC neste sistema, FFmpeg não será carregado; executando detecção automática...");
            }
            "nvenc" | "ffmpeg" => {
                match nvenc::FfmpegNvencEncoder::try_new(target_fps, is_screen_content) {
                    Ok(enc) => {
                        info!("🚀 [VIDEO CODEC FACTORY] Codificador manual NVIDIA NVENC ativado com sucesso!");
                        return Box::new(enc);
                    }
                    Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] NVENC manual indisponível ({}), executando detecção automática...", e),
                }
            }
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            "vaapi" => match vaapi::VaapiEncoder::try_new(target_fps, is_screen_content) {
                Ok(enc) => {
                    info!("🚀 [VIDEO CODEC FACTORY] Codificador manual VA-API (GPU) ativado com sucesso!");
                    return Box::new(enc);
                }
                Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] VA-API manual indisponível ({}), executando detecção automática...", e),
            },
            "amf" => {
                #[cfg(target_os = "windows")]
                {
                    match amd_amf::AmdAmfZeroCopyEncoder::try_new(target_fps, is_screen_content) {
                        Ok(enc) => {
                            info!("🚀 [VIDEO CODEC FACTORY] Codificador manual AMD AMF Zero-Copy ativado com sucesso!");
                            return Box::new(enc);
                        }
                        Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] AMF Zero-Copy manual falhou ({}), executando detecção automática...", e),
                    }
                }
            }
            "wmf" => {
                #[cfg(target_os = "windows")]
                {
                    match wmf::WmfGpuEncoder::try_new(target_fps, is_screen_content) {
                        Ok(enc) => {
                            info!("🚀 [VIDEO CODEC FACTORY] Codificador manual WMF GPU ativado com sucesso!");
                            return Box::new(enc);
                        }
                        Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] WMF GPU manual indisponível ({}), executando detecção automática...", e),
                    }
                }
            }
            "openh264" => {
                info!("🎯 [VIDEO CODEC FACTORY] Inicializando Cisco OpenH264 CPU manual...");
                match OpenH264Encoder::new(target_fps, is_screen_content) {
                    Ok(enc) => {
                        info!("🚀 [VIDEO CODEC FACTORY] Cisco OpenH264 CPU manual ativado com sucesso!");
                        return Box::new(enc);
                    }
                    Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] OpenH264 CPU manual falhou ({}), tentando novamente...", e),
                }
            }
            _ => {}
        }
    }

    #[cfg(target_os = "windows")]
    {
        let hw = detect_gpu_hardware();
        info!("🎯 [VIDEO CODEC FACTORY] Hardware detectado no sistema: {:?}", hw);

        if let GpuHardwareType::AmdAmf = hw {
            info!("🎯 [VIDEO CODEC FACTORY] GPU AMD Detectada! Inicializando AMF Native Zero-Copy Engine (AMFVideoConverter + VCE, 0% CPU)...");
            match amd_amf::AmdAmfZeroCopyEncoder::try_new(target_fps, is_screen_content) {
                Ok(enc) => {
                    info!("🚀 [VIDEO CODEC FACTORY] AMF Native Zero-Copy Engine ativado com sucesso (Placa: {})!", enc.gpu_name);
                    return Box::new(enc);
                }
                Err(e) => {
                    warn!("⚠️ [VIDEO CODEC FACTORY] AMF Native indisponível ({}), usando fallback...", e);
                }
            }
        }

        info!("🎯 [VIDEO CODEC FACTORY] Tentando Hardware GPU NVIDIA NVENC Engine (OBS / Sunshine Grade)...");
        match nvenc::FfmpegNvencEncoder::try_new(target_fps, is_screen_content) {
            Ok(enc) => {
                info!("🚀 [VIDEO CODEC FACTORY] NVIDIA NVENC Engine ativado com sucesso!");
                return Box::new(enc);
            }
            Err(e) => {
                warn!("⚠️ [VIDEO CODEC FACTORY] NVIDIA NVENC indisponível ({}), usando fallback OpenH264 SIMD...", e);
            }
        }
    }

    // The CPU choice stays on the CPU: no GPU engine is tried (or loaded) after it.
    #[cfg(target_os = "linux")]
    if manual_selection != "openh264" {
        let hw = detect_gpu_hardware();
        info!("🎯 [VIDEO CODEC FACTORY] Hardware detectado no sistema: {:?}", hw);

        // A manual "vaapi" has just been tried above.
        #[cfg(feature = "vaapi")]
        if manual_selection != "vaapi" {
            info!("🎯 [VIDEO CODEC FACTORY] Tentando VA-API (GPU AMD / Intel) via GStreamer...");
            match vaapi::VaapiEncoder::try_new(target_fps, is_screen_content) {
                Ok(enc) => {
                    info!("🚀 [VIDEO CODEC FACTORY] VA-API (GPU) ativado com sucesso!");
                    return Box::new(enc);
                }
                Err(e) => warn!("⚠️ [VIDEO CODEC FACTORY] VA-API indisponível ({}), tentando o próximo...", e),
            }
        }

        // The FFmpeg loader writes AVCodecContext fields at fixed offsets and only knows
        // h264_nvenc: without NVENC on the machine it cannot work.
        if hw == GpuHardwareType::NvidiaNvenc {
            info!("🎯 [VIDEO CODEC FACTORY] Tentando Hardware GPU Engine via FFmpeg (NVENC / AMF / QSV)...");
            match nvenc::FfmpegNvencEncoder::try_new(target_fps, is_screen_content) {
                Ok(enc) => {
                    info!("🚀 [VIDEO CODEC FACTORY] Hardware GPU Engine via FFmpeg ativado com sucesso!");
                    return Box::new(enc);
                }
                Err(e) => {
                    warn!("⚠️ [VIDEO CODEC FACTORY] FFmpeg GPU indisponível ({}), usando fallback OpenH264...", e);
                }
            }
        } else {
            info!("🎯 [VIDEO CODEC FACTORY] Sem NVENC neste sistema: FFmpeg não será tentado, usando OpenH264...");
        }
    }

    info!("🎯 [VIDEO CODEC FACTORY] Inicializando OpenH264 SIMD AVX2 (Universal CPU Engine)...");
    match OpenH264Encoder::new(target_fps, is_screen_content) {
        Ok(enc) => {
            info!("🚀 [VIDEO CODEC FACTORY] Encoder {} ativado com sucesso ({} threads, {:.1} Mbps)!",
                enc.name(), enc.num_threads, enc.get_bitrate_bps() as f64 / 1_000_000.0);
            Box::new(enc)
        }
        Err(e) => {
            warn!("⚠️ [VIDEO CODEC FACTORY] Falha ao inicializar OpenH264: {}. Tentando modo básico...", e);
            Box::new(OpenH264Encoder::new(target_fps, false).expect("Falha crítica no encoder H.264"))
        }
    }
}

/// An encoder that reads pictures straight from GPU textures (NV12, BT.709 limited range,
/// on the capture's D3D11 device): nothing goes through the CPU.
#[cfg(target_os = "windows")]
pub trait GpuVideoEncoder: Send {
    fn encode_texture(&mut self, texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Result<Vec<u8>, String>;
    fn force_idr(&mut self);
    fn set_bitrate(&mut self, bitrate_bps: u32);
    fn name(&self) -> &'static str;
}

#[cfg(target_os = "windows")]
impl GpuVideoEncoder for nvenc_d3d11::NvencD3d11Encoder {
    fn encode_texture(&mut self, texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Result<Vec<u8>, String> {
        nvenc_d3d11::NvencD3d11Encoder::encode_texture(self, texture)
    }
    fn force_idr(&mut self) {
        nvenc_d3d11::NvencD3d11Encoder::force_idr(self)
    }
    fn set_bitrate(&mut self, bitrate_bps: u32) {
        if let Err(e) = nvenc_d3d11::NvencD3d11Encoder::set_bitrate(self, bitrate_bps) {
            warn!("⚠️ [NVENC D3D11] Não foi possível mudar a taxa: {}", e);
        }
    }
    fn name(&self) -> &'static str {
        "NVIDIA NVENC (GPU, sem cópia)"
    }
}

#[cfg(target_os = "windows")]
impl GpuVideoEncoder for wmf::WmfGpuEncoder {
    fn encode_texture(&mut self, texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Result<Vec<u8>, String> {
        wmf::WmfGpuEncoder::encode_texture(self, texture)
    }
    fn force_idr(&mut self) {
        VideoEncoder::force_intra_frame(self)
    }
    fn set_bitrate(&mut self, bitrate_bps: u32) {
        if VideoEncoder::get_bitrate_bps(self) != bitrate_bps {
            VideoEncoder::set_bitrate_bps(self, bitrate_bps)
        }
    }
    fn name(&self) -> &'static str {
        "Media Foundation (GPU, sem cópia)"
    }
}

#[cfg(target_os = "windows")]
impl GpuVideoEncoder for amd_amf::AmfD3d11Encoder {
    fn encode_texture(&mut self, texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Result<Vec<u8>, String> {
        amd_amf::AmfD3d11Encoder::encode_texture(self, texture)
    }
    fn force_idr(&mut self) {
        amd_amf::AmfD3d11Encoder::force_idr(self)
    }
    fn set_bitrate(&mut self, bitrate_bps: u32) {
        amd_amf::AmfD3d11Encoder::set_bitrate(self, bitrate_bps)
    }
    fn name(&self) -> &'static str {
        "AMD AMF (GPU, sem cópia)"
    }
}

/// Whether some encoder can read textures of a device from this GPU vendor (PCI id), given
/// the encoder chosen in the settings: NVENC on NVIDIA, Quick Sync (through Media
/// Foundation) on Intel. AMF reads textures only when an AMD GPU is chosen by hand,
/// until it has run on AMD hardware.
#[cfg(target_os = "windows")]
pub fn gpu_encoder_supports(vendor_id: u32) -> bool {
    use crate::screen_capture::gpu_capture::{parse_choice, Choice};
    let choice = parse_choice(&crate::screen_capture::gpu_tx::encoder_choice());
    choice != Choice::Cpu && (matches!(vendor_id, 0x10de | 0x8086)
        || (vendor_id == 0x1002 && matches!(choice, Choice::Gpu { vendor: 0x1002, .. })))
}

/// The texture encoder for `device` (the capture's), for pictures of this size.
#[cfg(target_os = "windows")]
pub fn create_gpu_encoder(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    vendor_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<Box<dyn GpuVideoEncoder>, String> {
    if vendor_id == 0x10de {
        let enc = nvenc_d3d11::NvencD3d11Encoder::new(device, width, height, fps, bitrate_bps)?;
        info!("🚀 [VIDEO CODEC FACTORY] NVENC direto pela GPU ativado ({}x{} {} FPS, {:.1} Mbps), sem passar quadros pela CPU", width, height, fps, bitrate_bps as f64 / 1e6);
        return Ok(Box::new(enc));
    }
    if vendor_id == 0x1002 && gpu_encoder_supports(vendor_id) {
        let enc = amd_amf::AmfD3d11Encoder::new(device, width, height, fps, bitrate_bps)?;
        info!("🚀 [VIDEO CODEC FACTORY] AMD AMF pela GPU ativado ({}x{} {} FPS, {:.1} Mbps), sem passar quadros pela CPU", width, height, fps, bitrate_bps as f64 / 1e6);
        return Ok(Box::new(enc));
    }
    let enc = wmf::WmfGpuEncoder::for_device(device, vendor_id, width, height, fps, bitrate_bps).map_err(|e| e.to_string())?;
    info!("🚀 [VIDEO CODEC FACTORY] '{}' pela GPU ativado ({}x{} {} FPS, {:.1} Mbps), sem passar quadros pela CPU", enc.gpu_name, width, height, fps, enc.bitrate_bps as f64 / 1e6);
    Ok(Box::new(enc))
}
