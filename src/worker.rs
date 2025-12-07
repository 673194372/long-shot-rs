//! Thread B: Worker Thread
//! Handles Wayland screencopy and OpenCV image stitching.

use crate::capture::ScreenCapturer;
use crate::stitch::ImageStitcher;
use crate::types::{CaptureRegion, GuiCommand, InputEvent, StitchParams, WorkerEvent};
use crossbeam_channel::{Receiver, Sender, select};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Minimum interval between captures (milliseconds)
const MIN_CAPTURE_INTERVAL_MS: u64 = 100;

/// Delay after scroll event before capturing (milliseconds)
/// This allows the screen content to settle after scrolling
const SCROLL_SETTLE_DELAY_MS: u64 = 80;

/// Worker thread that coordinates capture and stitching
pub struct Worker {
    /// Region to capture
    region: CaptureRegion,
    /// Input events receiver
    input_rx: Receiver<InputEvent>,
    /// GUI commands receiver
    gui_rx: Receiver<GuiCommand>,
    /// Worker output sender
    worker_tx: Sender<WorkerEvent>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Stitching parameters
    stitch_params: StitchParams,
}

impl Worker {
    pub fn new(
        region: CaptureRegion,
        input_rx: Receiver<InputEvent>,
        gui_rx: Receiver<GuiCommand>,
        worker_tx: Sender<WorkerEvent>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            region,
            input_rx,
            gui_rx,
            worker_tx,
            shutdown,
            stitch_params: StitchParams::default(),
        }
    }

    /// Run the worker (blocking)
    pub fn run(&mut self) {
        info!("Worker thread starting...");

        // Initialize screen capturer
        let mut capturer = match ScreenCapturer::new() {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to initialize screen capture: {}", e);
                let _ = self
                    .worker_tx
                    .send(WorkerEvent::Error(format!("Capture init failed: {}", e)));
                return;
            }
        };

        info!("Screen capturer initialized");
        info!(
            "Capture region: {}x{} at ({}, {})",
            self.region.width, self.region.height, self.region.x, self.region.y
        );

        // Initialize stitcher
        let mut stitcher = ImageStitcher::new(self.stitch_params);

        // Capture initial frame
        match capturer.capture_region(&self.region) {
            Ok(frame) => {
                if let Err(e) = stitcher.process_frame(&frame) {
                    error!("Failed to process initial frame: {}", e);
                } else {
                    info!("Initial frame captured");
                    self.send_preview(&stitcher);
                }
            }
            Err(e) => {
                error!("Failed to capture initial frame: {}", e);
                let _ = self
                    .worker_tx
                    .send(WorkerEvent::Error(format!("Initial capture failed: {}", e)));
            }
        }

        let mut last_capture = Instant::now();
        let min_interval = Duration::from_millis(MIN_CAPTURE_INTERVAL_MS);
        let settle_delay = Duration::from_millis(SCROLL_SETTLE_DELAY_MS);
        let mut pending_scroll = false;
        let mut last_scroll_time = Instant::now();

        // Main processing loop
        while !self.shutdown.load(Ordering::Relaxed) {
            // Check if we have a pending scroll that has settled
            if pending_scroll {
                let since_last_scroll = Instant::now().duration_since(last_scroll_time);
                if since_last_scroll >= settle_delay {
                    pending_scroll = false;
                    
                    // Now capture after scrolling has stopped
                    debug!("Capturing after scroll settled ({}ms)", since_last_scroll.as_millis());
                    
                    match capturer.capture_region(&self.region) {
                        Ok(frame) => {
                            match stitcher.process_frame(&frame) {
                                Ok(stitched) => {
                                    if stitched {
                                        self.send_preview(&stitcher);
                                    }
                                }
                                Err(e) => {
                                    warn!("Stitch error: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Capture error: {}", e);
                        }
                    }
                    last_capture = Instant::now();
                }
            }
            
            // Use a short timeout when we have pending scroll
            let timeout = if pending_scroll {
                Duration::from_millis(10)
            } else {
                Duration::from_millis(100)
            };
            
            select! {
                recv(self.input_rx) -> msg => {
                    match msg {
                        Ok(InputEvent::ScrollDetected { direction }) => {
                            let now = Instant::now();
                            
                            // Rate limit
                            if now.duration_since(last_capture) < min_interval {
                                debug!("Rate limited, but marking pending scroll");
                            }
                            
                            debug!("Scroll event received: direction={}", direction);
                            
                            // Mark that we have a pending scroll and update the time
                            // This will batch rapid scroll events together
                            pending_scroll = true;
                            last_scroll_time = now;
                        }
                        Ok(InputEvent::Shutdown) => {
                            info!("Received shutdown from input thread");
                            break;
                        }
                        Err(_) => {
                            // Channel closed
                            break;
                        }
                    }
                }
                recv(self.gui_rx) -> msg => {
                    match msg {
                        Ok(GuiCommand::Shutdown) => {
                            info!("Received shutdown from GUI");
                            break;
                        }
                        Ok(GuiCommand::RequestSave) => {
                            // GUI requests save - handled by GUI directly now
                        }
                        Ok(GuiCommand::StartCapture(_)) => {
                            // Reset stitcher for new capture
                            stitcher.reset();
                        }
                        Err(_) => {
                            // Channel closed
                            break;
                        }
                    }
                }
                default(timeout) => {
                    // Periodic check for shutdown and pending scroll processing
                }
            }
        }

        info!("Worker thread exiting");
        let _ = self.worker_tx.send(WorkerEvent::CaptureComplete);
    }

    /// Send full-resolution image to GUI (preview will scale for display)
    fn send_preview(&self, stitcher: &ImageStitcher) {
        // 发送原始尺寸图像，预览窗口会自动缩放显示
        // 这样保存/复制时使用的是完整分辨率
        if let Some((data, width, height)) = stitcher.get_result_rgba() {
            let _ = self.worker_tx.send(WorkerEvent::ImageUpdated {
                data,
                width,
                height,
            });
        }
    }
}

/// Start the worker thread
pub fn start_worker_thread(
    region: CaptureRegion,
    input_rx: Receiver<InputEvent>,
    gui_rx: Receiver<GuiCommand>,
    worker_tx: Sender<WorkerEvent>,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut worker = Worker::new(region, input_rx, gui_rx, worker_tx, shutdown);
        worker.run();
    })
}
