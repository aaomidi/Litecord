//! H.264 decoding on the GPU. The CPU only parses headers and tracks references
//! (h264_state); the platform backend hands each picture to the GPU's fixed-function
//! decoder, and windows showing the stream draw the picture straight from the GPU
//! (layers). Streams or machines it cannot handle fall back to the CPU decoder.

pub mod h264_state;
#[cfg(windows)]
pub mod device;
#[cfg(windows)]
pub mod dxva;
#[cfg(windows)]
pub mod layers;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod gl_present;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod vaapi;

use std::collections::HashSet;
use std::sync::Mutex;

use slint::{Rgba8Pixel, SharedPixelBuffer};

#[derive(Debug)]
pub enum HwError {
    /// This stream or machine cannot use the GPU decoder: decode on the CPU.
    Unsupported(String),
    /// Wait for the next keyframe (lost picture, or the stream has not reached one).
    NeedKeyframe(String),
    /// The GPU failed (driver reset, device lost): decode on the CPU.
    Device(String),
}

/// A picture decoded on the GPU.
pub enum GpuPicture {
    /// Drawn straight into the windows that show the stream, or not needed by them (its
    /// height).
    Shown(u32),
    /// No window shows it on the GPU: copied back as RGBA (the stream's width, height).
    Rgba(SharedPixelBuffer<Rgba8Pixel>, u32, u32),
}

/// Streams currently decoded on the GPU (the UI turns window layers on for them).
static GPU_PEERS: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

pub fn peer_on_gpu(peer: u64) -> bool {
    GPU_PEERS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.contains(&peer)).unwrap_or(false)
}

fn mark_peer(peer: u64, on: bool) {
    let mut g = GPU_PEERS.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if on {
        set.insert(peer);
    } else {
        set.remove(&peer);
    }
}

pub struct GpuDecoder {
    peer: u64,
    #[cfg(windows)]
    inner: dxva::DxvaDecoder,
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    inner: vaapi::VaapiDecoder,
    /// When the last access unit was done with (Linux: a new one right after it means
    /// the decode thread has a queue to work off).
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    last_done: Option<std::time::Instant>,
}

/// The next access unit starting this soon after the last one was waiting in the queue.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
const BACK_TO_BACK: std::time::Duration = std::time::Duration::from_millis(1);

impl GpuDecoder {
    pub fn new(peer: u64) -> Result<Self, HwError> {
        #[cfg(windows)]
        {
            Ok(Self { peer, inner: dxva::DxvaDecoder::new()? })
        }
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        {
            Ok(Self { peer, inner: vaapi::VaapiDecoder::new()?, last_done: None })
        }
        #[cfg(not(any(windows, all(target_os = "linux", feature = "vaapi"))))]
        {
            let _ = peer;
            Err(HwError::Unsupported("no GPU decoder on this platform yet".into()))
        }
    }

    /// Decodes one access unit. The newest picture due on screen is drawn into the windows
    /// showing this stream on the GPU, or else copied back at the size `fit(width,
    /// height)` chooses.
    pub fn decode(&mut self, au: &[u8], fit: impl Fn(u32, u32) -> (u32, u32)) -> Result<Option<GpuPicture>, HwError> {
        #[cfg(windows)]
        {
            let Some(frame) = self.inner.decode(au)?.pop() else { return Ok(None) };
            mark_peer(self.peer, true);
            if layers::present(self.peer, &frame, self.inner.shared()) {
                return Ok(Some(GpuPicture::Shown(frame.info.height)));
            }
            let (w, h) = fit(frame.info.width, frame.info.height);
            Ok(Some(GpuPicture::Rgba(self.inner.read_rgba(&frame, w, h)?, frame.info.width, frame.info.height)))
        }
        // Linux: vapostproc scales and converts on the GPU. With the GPU renderer the
        // picture stays there and the windows draw it from the decoder's buffer
        // (gl_present). Otherwise it comes back as RGBA, unless the UI thread has not
        // taken the previous one yet and it or the decode thread is behind: a picture
        // that would only replace that one unseen is decoded and left on the GPU.
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        {
            let busy = self.last_done.is_some_and(|t| t.elapsed() < BACK_TO_BACK);
            let read_back = !super::video_pipeline::rx_ui_behind(busy);
            let decoded = self.inner.decode(au, fit, read_back);
            self.last_done = Some(std::time::Instant::now());
            let Some(picture) = decoded? else { return Ok(None) };
            match picture.output {
                vaapi::Output::Dmabuf(sample) => {
                    if !gl_present::offer(self.peer, sample, picture.height) {
                        // Not an error of the decoder: the stream stays on it, and its
                        // next keyframe starts a pipeline that returns pixels.
                        return Err(HwError::NeedKeyframe("GPU presentation ended".into()));
                    }
                    mark_peer(self.peer, true);
                    Ok(Some(GpuPicture::Shown(picture.height)))
                }
                vaapi::Output::Rgba(Some(rgba)) => {
                    gl_present::clear(self.peer);
                    Ok(Some(GpuPicture::Rgba(rgba, picture.width, picture.height)))
                }
                vaapi::Output::Rgba(None) => Ok(Some(GpuPicture::Shown(picture.height))),
            }
        }
        #[cfg(not(any(windows, all(target_os = "linux", feature = "vaapi"))))]
        {
            let _ = (au, fit);
            Err(HwError::Unsupported("no GPU decoder on this platform yet".into()))
        }
    }
}

impl Drop for GpuDecoder {
    fn drop(&mut self) {
        mark_peer(self.peer, false);
        // Its last picture is not offered to a window again (the stream may go on with
        // the CPU decoder).
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        gl_present::clear(self.peer);
    }
}
