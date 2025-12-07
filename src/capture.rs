//! Wayland screen capture using wlr-screencopy-unstable-v1 protocol.
//! Implements SHM buffer management for frame capture.

use crate::types::{CaptureRegion, RawFrame};
use anyhow::{anyhow, Context, Result};
use log::{debug, error, info};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::ptr::NonNull;
use tempfile::tempfile;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// Format info received from screencopy
#[derive(Debug, Clone, Default)]
struct BufferFormat {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

/// State of frame capture
#[derive(Debug, Clone, Copy, PartialEq)]
enum FrameState {
    Pending,
    BufferDone,
    Ready,
    Failed,
}

/// Wayland state for screencopy
struct WaylandState {
    // Globals
    shm: Option<wl_shm::WlShm>,
    screencopy_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    outputs: Vec<wl_output::WlOutput>,

    // Frame capture state
    buffer_format: BufferFormat,
    frame_state: FrameState,
    frame_flags: u32,

    // SHM buffer
    shm_pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    buffer_data: Option<MappedBuffer>,
}

impl WaylandState {
    fn new() -> Self {
        Self {
            shm: None,
            screencopy_manager: None,
            outputs: Vec::new(),
            buffer_format: BufferFormat::default(),
            frame_state: FrameState::Pending,
            frame_flags: 0,
            shm_pool: None,
            buffer: None,
            buffer_data: None,
        }
    }
}

/// Memory-mapped buffer for SHM
struct MappedBuffer {
    ptr: NonNull<u8>,
    size: usize,
    fd: OwnedFd,
}

impl MappedBuffer {
    fn new(size: usize) -> Result<Self> {
        // Create anonymous temporary file
        let file = tempfile().context("Failed to create temp file for SHM")?;
        file.set_len(size as u64)
            .context("Failed to set SHM file size")?;

        let fd = OwnedFd::from(file);

        // Memory map the file
        let ptr = unsafe {
            mmap(
                None,
                NonZeroUsize::new(size).ok_or_else(|| anyhow!("Invalid buffer size"))?,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
            .context("Failed to mmap SHM buffer")?
        };

        Ok(Self {
            ptr: ptr.cast(),
            size,
            fd,
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    #[allow(dead_code)]
    fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(self.ptr.cast(), self.size);
        }
    }
}

// Implement Dispatch for registry
impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    debug!("Found wl_shm (version {})", version);
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                "wl_output" => {
                    debug!("Found wl_output (version {})", version);
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    );
                    state.outputs.push(output);
                }
                "zwlr_screencopy_manager_v1" => {
                    debug!("Found zwlr_screencopy_manager_v1 (version {})", version);
                    state.screencopy_manager = Some(
                        registry
                            .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                                name,
                                version.min(3),
                                qh,
                                (),
                            ),
                    );
                }
                _ => {}
            }
        }
    }
}

// Implement Dispatch for SHM
impl Dispatch<wl_shm::WlShm, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _shm: &wl_shm::WlShm,
        event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event {
            debug!("SHM format available: {:?}", format);
        }
    }
}

// Delegate for output (we don't need events)
delegate_noop!(WaylandState: ignore wl_output::WlOutput);
delegate_noop!(WaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandState: ignore wl_buffer::WlBuffer);
delegate_noop!(WaylandState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

// Implement Dispatch for screencopy frame
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                debug!(
                    "Frame buffer info: format={:?}, {}x{}, stride={}",
                    format, width, height, stride
                );

                // We prefer ARGB8888 or XRGB8888
                if let WEnum::Value(fmt) = format {
                    let fmt_val = fmt as u32;
                    // ARGB8888 = 0, XRGB8888 = 1
                    if fmt_val == 0 || fmt_val == 1 {
                        state.buffer_format = BufferFormat {
                            format: fmt_val,
                            width,
                            height,
                            stride,
                        };
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                debug!("Frame flags: {:?}", flags);
                if let WEnum::Value(f) = flags {
                    state.frame_flags = f.bits();
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                debug!("Buffer format negotiation done");
                state.frame_state = FrameState::BufferDone;

                // Create SHM buffer with the format we received
                if state.buffer_format.stride > 0 {
                    let size = (state.buffer_format.stride * state.buffer_format.height) as usize;

                    match MappedBuffer::new(size) {
                        Ok(mapped) => {
                            if let Some(ref shm) = state.shm {
                                // Create SHM pool
                                let pool = shm.create_pool(
                                    mapped.fd.as_fd(),
                                    size as i32,
                                    qh,
                                    (),
                                );

                                // Create buffer from pool
                                let buffer = pool.create_buffer(
                                    0,
                                    state.buffer_format.width as i32,
                                    state.buffer_format.height as i32,
                                    state.buffer_format.stride as i32,
                                    match state.buffer_format.format {
                                        0 => wl_shm::Format::Argb8888,
                                        _ => wl_shm::Format::Xrgb8888,
                                    },
                                    qh,
                                    (),
                                );

                                // Copy the buffer to frame
                                frame.copy(&buffer);

                                state.shm_pool = Some(pool);
                                state.buffer = Some(buffer);
                                state.buffer_data = Some(mapped);

                                debug!("SHM buffer created and copy requested");
                            }
                        }
                        Err(e) => {
                            error!("Failed to create SHM buffer: {}", e);
                            state.frame_state = FrameState::Failed;
                        }
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                debug!("Frame ready!");
                state.frame_state = FrameState::Ready;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                error!("Frame capture failed!");
                state.frame_state = FrameState::Failed;
            }
            _ => {}
        }
    }
}

/// Screen capturer using Wayland screencopy protocol
pub struct ScreenCapturer {
    #[allow(dead_code)]
    connection: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl ScreenCapturer {
    /// Create a new screen capturer
    pub fn new() -> Result<Self> {
        let connection = Connection::connect_to_env().context("Failed to connect to Wayland")?;

        let display = connection.display();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();

        let mut state = WaylandState::new();

        // Get registry and bind globals
        let _registry = display.get_registry(&qh, ());

        // Roundtrip to get globals
        queue.roundtrip(&mut state)?;

        // Verify we have required globals
        if state.shm.is_none() {
            return Err(anyhow!("Compositor does not support wl_shm"));
        }

        if state.screencopy_manager.is_none() {
            return Err(anyhow!(
                "Compositor does not support wlr-screencopy-unstable-v1.\n\
                This protocol is typically supported by wlroots-based compositors \n\
                (Sway, Hyprland, etc.)"
            ));
        }

        if state.outputs.is_empty() {
            return Err(anyhow!("No outputs found"));
        }

        info!(
            "Wayland capture initialized with {} output(s)",
            state.outputs.len()
        );

        Ok(Self {
            connection,
            queue,
            state,
        })
    }

    /// Capture a region of the screen
    pub fn capture_region(&mut self, region: &CaptureRegion) -> Result<RawFrame> {
        let qh = self.queue.handle();

        // Reset state
        self.state.frame_state = FrameState::Pending;
        self.state.buffer_format = BufferFormat::default();

        // Clean up previous buffers
        if let Some(buffer) = self.state.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.state.shm_pool.take() {
            pool.destroy();
        }
        self.state.buffer_data = None;

        // Get first output (TODO: support multiple monitors)
        let output = self
            .state
            .outputs
            .first()
            .ok_or_else(|| anyhow!("No output available"))?
            .clone();

        // Request frame capture for region
        let manager = self
            .state
            .screencopy_manager
            .as_ref()
            .ok_or_else(|| anyhow!("Screencopy manager not available"))?;

        let _frame = manager.capture_output_region(
            0, // no cursor
            &output,
            region.x,
            region.y,
            region.width as i32,
            region.height as i32,
            &qh,
            (),
        );

        // Wait for frame to complete
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);

        while self.state.frame_state != FrameState::Ready
            && self.state.frame_state != FrameState::Failed
        {
            if start.elapsed() > timeout {
                return Err(anyhow!("Frame capture timed out"));
            }

            self.queue
                .blocking_dispatch(&mut self.state)
                .context("Wayland dispatch failed")?;
        }

        if self.state.frame_state == FrameState::Failed {
            return Err(anyhow!("Frame capture failed"));
        }

        // Extract frame data
        let buffer_data = self
            .state
            .buffer_data
            .as_ref()
            .ok_or_else(|| anyhow!("No buffer data available"))?;

        let format = &self.state.buffer_format;

        // Handle Y-invert flag (bit 0)
        let mut data = buffer_data.as_slice().to_vec();
        if self.state.frame_flags & 1 != 0 {
            debug!("Applying Y-invert transformation");
            let stride = format.stride as usize;
            let height = format.height as usize;
            let mut flipped = vec![0u8; data.len()];
            for y in 0..height {
                let src_row = &data[y * stride..(y + 1) * stride];
                let dst_row = &mut flipped[(height - 1 - y) * stride..(height - y) * stride];
                dst_row.copy_from_slice(src_row);
            }
            data = flipped;
        }

        Ok(RawFrame {
            data,
            width: format.width,
            height: format.height,
            stride: format.stride,
        })
    }
}

/// Get region selection from slurp
pub fn select_region_with_slurp() -> Result<CaptureRegion> {
    use std::process::Command;

    info!("Launching slurp for region selection...");

    let output = Command::new("slurp")
        .output()
        .context("Failed to run slurp. Is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("slurp failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    CaptureRegion::from_slurp(&stdout)
}
