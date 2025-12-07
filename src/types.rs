//! Shared types and channel definitions for inter-thread communication.

use crossbeam_channel::{Receiver, Sender};

/// Region selected by user (from slurp)
#[derive(Debug, Clone, Copy)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl CaptureRegion {
    /// Parse slurp output format: "x,y wxh"
    pub fn from_slurp(output: &str) -> anyhow::Result<Self> {
        let output = output.trim();
        // Format: "x,y wxh"
        let parts: Vec<&str> = output.split_whitespace().collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid slurp output format: {}", output);
        }

        let coords: Vec<&str> = parts[0].split(',').collect();
        if coords.len() != 2 {
            anyhow::bail!("Invalid coordinates format: {}", parts[0]);
        }

        let dims: Vec<&str> = parts[1].split('x').collect();
        if dims.len() != 2 {
            anyhow::bail!("Invalid dimensions format: {}", parts[1]);
        }

        Ok(Self {
            x: coords[0].parse()?,
            y: coords[1].parse()?,
            width: dims[0].parse()?,
            height: dims[1].parse()?,
        })
    }
}

/// Message from input thread to worker thread
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Scroll detected (direction: positive = down, negative = up)
    ScrollDetected { direction: i32 },
    /// Stop signal
    Shutdown,
}

/// Message from worker thread to GUI thread
#[derive(Debug)]
pub enum WorkerEvent {
    /// New stitched image available (RGBA data, width, height)
    ImageUpdated {
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Status message
    #[allow(dead_code)]
    Status(String),
    /// Error occurred
    Error(String),
    /// Capture completed
    CaptureComplete,
}

/// Message from GUI thread to worker thread
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum GuiCommand {
    /// Start capture session with given region
    StartCapture(CaptureRegion),
    /// Request current stitched image for saving
    RequestSave,
    /// Shutdown worker
    Shutdown,
}

/// Channels for inter-thread communication
pub struct Channels {
    /// Input -> Worker: trigger captures
    pub input_tx: Sender<InputEvent>,
    pub input_rx: Receiver<InputEvent>,

    /// Worker -> GUI: image updates
    pub worker_tx: Sender<WorkerEvent>,
    pub worker_rx: Receiver<WorkerEvent>,

    /// GUI -> Worker: commands
    pub gui_tx: Sender<GuiCommand>,
    pub gui_rx: Receiver<GuiCommand>,
}

impl Channels {
    pub fn new() -> Self {
        let (input_tx, input_rx) = crossbeam_channel::unbounded();
        let (worker_tx, worker_rx) = crossbeam_channel::unbounded();
        let (gui_tx, gui_rx) = crossbeam_channel::unbounded();

        Self {
            input_tx,
            input_rx,
            worker_tx,
            worker_rx,
            gui_tx,
            gui_rx,
        }
    }
}

/// Stitching algorithm parameters
#[derive(Debug, Clone, Copy)]
pub struct StitchParams {
    /// Ignore top portion of frame (navigation bars)
    pub ignore_y_top: f64,
    /// Ignore bottom portion of frame (status bars)
    pub ignore_y_bottom: f64,
    /// Minimum match confidence threshold
    pub match_confidence: f64,
    /// Template height ratio (portion of frame to use as template)
    pub template_ratio: f64,
    /// Inertia constraint: max Y search range (pixels)
    pub max_search_range: i32,
}

impl Default for StitchParams {
    fn default() -> Self {
        Self {
            ignore_y_top: 0.15,
            ignore_y_bottom: 0.15,
            match_confidence: 0.5,
            template_ratio: 0.2,
            max_search_range: 500,
        }
    }
}

/// Raw frame data from Wayland capture
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// BGRA pixel data
    pub data: Vec<u8>,
    /// Frame width
    pub width: u32,
    /// Frame height
    pub height: u32,
    /// Stride (bytes per row)
    pub stride: u32,
}

impl RawFrame {
    /// Convert BGRA to RGBA
    #[allow(dead_code)]
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(self.data.len());
        for chunk in self.data.chunks(4) {
            if chunk.len() == 4 {
                rgba.push(chunk[2]); // R
                rgba.push(chunk[1]); // G
                rgba.push(chunk[0]); // B
                rgba.push(chunk[3]); // A
            }
        }
        rgba
    }
}
