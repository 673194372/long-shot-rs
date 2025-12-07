//! Thread A: Input Monitor
//! Monitors /dev/input/event* devices for scroll wheel events using evdev.

use crate::types::InputEvent;
use crossbeam_channel::Sender;
use evdev::{Device, InputEventKind, RelativeAxisType};
use log::{debug, error, info, warn};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Debounce interval for scroll events (milliseconds)
const DEBOUNCE_MS: u64 = 50;

/// Input monitor that watches for scroll wheel events
pub struct InputMonitor {
    tx: Sender<InputEvent>,
    shutdown: Arc<AtomicBool>,
}

impl InputMonitor {
    pub fn new(tx: Sender<InputEvent>, shutdown: Arc<AtomicBool>) -> Self {
        Self { tx, shutdown }
    }

    /// Find all input devices with scroll wheel capability
    fn find_scroll_devices() -> Vec<PathBuf> {
        let mut devices = Vec::new();

        let input_dir = PathBuf::from("/dev/input");
        let entries = match fs::read_dir(&input_dir) {
            Ok(e) => e,
            Err(e) => {
                error!("Cannot read /dev/input: {}", e);
                eprintln!(
                    "\n╔══════════════════════════════════════════════════════════════╗"
                );
                eprintln!(
                    "║  ERROR: Cannot access /dev/input devices.                    ║"
                );
                eprintln!(
                    "║                                                              ║"
                );
                eprintln!(
                    "║  Please try one of the following:                            ║"
                );
                eprintln!(
                    "║  1. Add your user to the 'input' group:                      ║"
                );
                eprintln!(
                    "║     sudo usermod -aG input $USER                             ║"
                );
                eprintln!(
                    "║     (then log out and log back in)                           ║"
                );
                eprintln!(
                    "║                                                              ║"
                );
                eprintln!(
                    "║  2. Run with sudo:                                           ║"
                );
                eprintln!(
                    "║     sudo long-shot-rs                                        ║"
                );
                eprintln!(
                    "╚══════════════════════════════════════════════════════════════╝\n"
                );
                return devices;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();

            // Only look at event* devices
            if !name.starts_with("event") {
                continue;
            }

            // Try to open and check capabilities
            match Device::open(&path) {
                Ok(device) => {
                    // Check if device supports relative wheel
                    if let Some(rel_caps) = device.supported_relative_axes() {
                        if rel_caps.contains(RelativeAxisType::REL_WHEEL)
                            || rel_caps.contains(RelativeAxisType::REL_WHEEL_HI_RES)
                        {
                            info!(
                                "Found scroll device: {} ({:?})",
                                device.name().unwrap_or("Unknown"),
                                path
                            );
                            devices.push(path);
                        }
                    }
                }
                Err(e) => {
                    // Permission denied is common, only warn for other errors
                    if e.kind() != std::io::ErrorKind::PermissionDenied {
                        debug!("Cannot open {:?}: {}", path, e);
                    }
                }
            }
        }

        if devices.is_empty() {
            warn!("No scroll-capable devices found!");
            eprintln!(
                "\n╔══════════════════════════════════════════════════════════════╗"
            );
            eprintln!(
                "║  WARNING: No scroll wheel devices found.                     ║"
            );
            eprintln!(
                "║                                                              ║"
            );
            eprintln!(
                "║  Make sure you have:                                         ║"
            );
            eprintln!(
                "║  - A mouse with a scroll wheel connected                     ║"
            );
            eprintln!(
                "║  - Permissions to read /dev/input/event* devices             ║"
            );
            eprintln!(
                "╚══════════════════════════════════════════════════════════════╝\n"
            );
        }

        devices
    }

    /// Run the input monitor (blocking)
    pub fn run(&self) {
        let devices = Self::find_scroll_devices();

        if devices.is_empty() {
            error!("No input devices available, input thread exiting");
            return;
        }

        // Spawn a monitoring thread for each device
        let mut handles = Vec::new();

        for device_path in devices {
            let tx = self.tx.clone();
            let shutdown = self.shutdown.clone();

            let handle = thread::spawn(move || {
                Self::monitor_device(device_path, tx, shutdown);
            });

            handles.push(handle);
        }

        // Wait for shutdown
        while !self.shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }

        info!("Input monitor shutting down");

        // Send shutdown signal
        let _ = self.tx.send(InputEvent::Shutdown);

        // Wait for all device threads
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// Monitor a single input device for scroll events
    fn monitor_device(path: PathBuf, tx: Sender<InputEvent>, shutdown: Arc<AtomicBool>) {
        let device = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                error!("Cannot open {:?}: {}", path, e);
                return;
            }
        };

        info!(
            "Monitoring device: {} ({:?})",
            device.name().unwrap_or("Unknown"),
            path
        );

        // Set device to non-blocking mode using fcntl
        let fd = device.as_raw_fd();
        if let Err(e) = fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)) {
            warn!("Cannot set non-blocking mode for {:?}: {}", path, e);
        }

        let mut last_scroll_time = Instant::now();
        let debounce_duration = Duration::from_millis(DEBOUNCE_MS);

        while !shutdown.load(Ordering::Relaxed) {
            // Try to fetch events using into_event_stream alternative
            // Since we set O_NONBLOCK, we need to handle reads manually
            match Self::read_events_nonblocking(&device) {
                Ok(events) => {
                    for event in events {
                        match event.kind() {
                            InputEventKind::RelAxis(RelativeAxisType::REL_WHEEL) => {
                                let now = Instant::now();

                                // Debounce: skip if too soon after last event
                                if now.duration_since(last_scroll_time) < debounce_duration {
                                    continue;
                                }

                                last_scroll_time = now;
                                let direction = event.value();

                                debug!("Scroll event: direction={}", direction);

                                if let Err(e) =
                                    tx.send(InputEvent::ScrollDetected { direction })
                                {
                                    error!("Cannot send scroll event: {}", e);
                                    return;
                                }
                            }
                            InputEventKind::RelAxis(RelativeAxisType::REL_WHEEL_HI_RES) => {
                                // High-resolution wheel events, aggregate them
                                let now = Instant::now();

                                if now.duration_since(last_scroll_time) < debounce_duration {
                                    continue;
                                }

                                // Hi-res wheel uses 120 units per notch
                                let value = event.value();
                                if value.abs() >= 60 {
                                    last_scroll_time = now;
                                    let direction = if value > 0 { 1 } else { -1 };

                                    debug!("Hi-res scroll event: direction={}", direction);

                                    if let Err(e) =
                                        tx.send(InputEvent::ScrollDetected { direction })
                                    {
                                        error!("Cannot send scroll event: {}", e);
                                        return;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No events available, sleep briefly
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    error!("Error reading from {:?}: {}", path, e);
                    break;
                }
            }
        }

        info!("Stopped monitoring {:?}", path);
    }

    /// Read events from device in non-blocking mode
    fn read_events_nonblocking(device: &Device) -> std::io::Result<Vec<evdev::InputEvent>> {
        use evdev::EventType;
        
        let fd = device.as_raw_fd();
        let mut events = Vec::new();
        
        // Size of input_event struct (24 bytes on 64-bit)
        const EVENT_SIZE: usize = 24;
        let mut buf = [0u8; EVENT_SIZE * 64]; // Read up to 64 events at once
        
        // Use nix to read from fd
        match nix::unistd::read(fd, &mut buf) {
            Ok(n) if n >= EVENT_SIZE => {
                let num_events = n / EVENT_SIZE;
                for i in 0..num_events {
                    let offset = i * EVENT_SIZE;
                    let event_bytes = &buf[offset..offset + EVENT_SIZE];
                    
                    // Parse the raw event
                    // struct input_event { time_t tv_sec, suseconds_t tv_usec, u16 type, u16 code, i32 value }
                    let event_type = u16::from_ne_bytes([event_bytes[16], event_bytes[17]]);
                    let event_code = u16::from_ne_bytes([event_bytes[18], event_bytes[19]]);
                    let event_value = i32::from_ne_bytes([event_bytes[20], event_bytes[21], event_bytes[22], event_bytes[23]]);
                    
                    // Create InputEvent - EventType is a newtype wrapper around u16
                    let evt = evdev::InputEvent::new_now(EventType(event_type), event_code, event_value);
                    events.push(evt);
                }
                Ok(events)
            }
            Ok(_) => Ok(events),
            Err(nix::errno::Errno::EAGAIN) => {
                Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "No events"))
            }
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
        }
    }
}

/// Start the input monitor thread
pub fn start_input_thread(
    tx: Sender<InputEvent>,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let monitor = InputMonitor::new(tx, shutdown);
        monitor.run();
    })
}
