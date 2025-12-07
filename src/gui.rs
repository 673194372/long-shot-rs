//! Thread C: GUI Main Thread
//! Implements the eframe/egui interface for preview and controls.

use crate::types::{GuiCommand, WorkerEvent};
use arboard::Clipboard;
use chrono::Local;
use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{self, ColorImage, ScrollArea, TextureHandle, TextureOptions};
use log::{error, info};
use std::path::PathBuf;

/// GUI Application state
pub struct LongShotApp {
    /// Channel to receive updates from worker
    worker_rx: Receiver<WorkerEvent>,
    /// Channel to send commands to worker
    gui_tx: Sender<GuiCommand>,

    /// Current preview texture
    texture: Option<TextureHandle>,
    /// Current image dimensions
    image_size: Option<(u32, u32)>,
    /// Raw image data for clipboard/save (RGBA)
    raw_image: Option<(Vec<u8>, u32, u32)>,

    /// Status message
    status: String,
    /// Frame count
    frame_count: u32,
    /// Total height
    total_height: i32,

    /// Is capturing active
    is_capturing: bool,

    /// Error message
    error_msg: Option<String>,

    /// Save path
    save_path: Option<String>,
}

impl LongShotApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        worker_rx: Receiver<WorkerEvent>,
        gui_tx: Sender<GuiCommand>,
    ) -> Self {
        // Configure egui style
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        cc.egui_ctx.set_style(style);

        Self {
            worker_rx,
            gui_tx,
            texture: None,
            image_size: None,
            raw_image: None,
            status: "Ready - Scroll to capture".to_string(),
            frame_count: 0,
            total_height: 0,
            is_capturing: true,
            error_msg: None,
            save_path: None,
        }
    }

    /// Process incoming worker events
    fn process_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.worker_rx.try_recv() {
            match event {
                WorkerEvent::ImageUpdated {
                    data,
                    width,
                    height,
                } => {
                    // Store raw image for later save/copy
                    self.raw_image = Some((data.clone(), width, height));
                    self.image_size = Some((width, height));

                    // Create texture
                    let image = ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &data,
                    );

                    self.texture = Some(ctx.load_texture(
                        "preview",
                        image,
                        TextureOptions::LINEAR,
                    ));

                    self.frame_count += 1;
                    self.total_height = height as i32;
                    self.status = format!(
                        "Frames: {} | Height: {}px",
                        self.frame_count, self.total_height
                    );
                }
                WorkerEvent::Status(msg) => {
                    self.status = msg;
                }
                WorkerEvent::Error(msg) => {
                    self.error_msg = Some(msg);
                }
                WorkerEvent::CaptureComplete => {
                    self.is_capturing = false;
                    self.status = format!(
                        "Capture complete - {} frames, {}px height",
                        self.frame_count, self.total_height
                    );
                }
            }
        }
    }

    /// Save image to file
    fn save_image(&mut self) {
        if let Some((ref data, width, height)) = self.raw_image {
            let timestamp = Local::now().format("%Y%m%d_%H%M%S");
            let filename = format!("longshot_{}.png", timestamp);

            // Try to use XDG pictures directory, fallback to current directory
            let save_dir = dirs::picture_dir()
                .or_else(|| dirs::home_dir())
                .unwrap_or_else(|| PathBuf::from("."));

            let path = save_dir.join(&filename);

            // Convert RGBA to image crate format and save
            match image::RgbaImage::from_raw(width, height, data.clone()) {
                Some(img) => {
                    match img.save(&path) {
                        Ok(_) => {
                            let path_str = path.to_string_lossy().to_string();
                            self.save_path = Some(path_str.clone());
                            self.status = format!("Saved to: {}", path_str);
                            info!("Image saved to: {}", path_str);
                        }
                        Err(e) => {
                            self.error_msg = Some(format!("Save failed: {}", e));
                            error!("Failed to save image: {}", e);
                        }
                    }
                }
                None => {
                    self.error_msg = Some("Failed to create image buffer".to_string());
                }
            }
        } else {
            self.error_msg = Some("No image to save".to_string());
        }
    }

    /// Copy image to clipboard
    fn copy_to_clipboard(&mut self) {
        if let Some((ref data, width, height)) = self.raw_image {
            match Clipboard::new() {
                Ok(mut clipboard) => {
                    let img = arboard::ImageData {
                        width: width as usize,
                        height: height as usize,
                        bytes: std::borrow::Cow::Borrowed(data),
                    };

                    match clipboard.set_image(img) {
                        Ok(_) => {
                            self.status = "Image copied to clipboard!".to_string();
                            info!("Image copied to clipboard");
                        }
                        Err(e) => {
                            self.error_msg = Some(format!("Clipboard error: {}", e));
                            error!("Failed to copy to clipboard: {}", e);
                        }
                    }
                }
                Err(e) => {
                    self.error_msg = Some(format!("Cannot access clipboard: {}", e));
                    error!("Failed to access clipboard: {}", e);
                }
            }
        } else {
            self.error_msg = Some("No image to copy".to_string());
        }
    }
}

impl eframe::App for LongShotApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process incoming events
        self.process_events(ctx);

        // Request continuous repaint while capturing
        if self.is_capturing {
            ctx.request_repaint();
        }

        // Top panel with controls
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("📸 Long Shot");
                ui.separator();

                // Save button
                if ui
                    .add_enabled(
                        self.raw_image.is_some(),
                        egui::Button::new("💾 Save"),
                    )
                    .clicked()
                {
                    self.save_image();
                }

                // Copy button
                if ui
                    .add_enabled(
                        self.raw_image.is_some(),
                        egui::Button::new("📋 Copy"),
                    )
                    .clicked()
                {
                    self.copy_to_clipboard();
                }

                ui.separator();

                // Status
                ui.label(&self.status);

                // Error indicator
                if self.error_msg.is_some() {
                    ui.colored_label(egui::Color32::RED, "⚠");
                }
            });
        });

        // Bottom panel with info
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some((_, w, h)) = &self.raw_image {
                    ui.label(format!("Size: {}×{}", w, h));
                }

                if let Some(ref path) = self.save_path {
                    ui.separator();
                    ui.label(format!("Last saved: {}", path));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_capturing {
                        ui.colored_label(egui::Color32::GREEN, "● Capturing");
                    } else {
                        ui.label("○ Idle");
                    }
                });
            });
        });

        // Central panel with scrollable image preview
        egui::CentralPanel::default().show(ctx, |ui| {
            // Error popup
            if let Some(ref msg) = self.error_msg.clone() {
                egui::Window::new("Error")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.colored_label(egui::Color32::RED, msg);
                        if ui.button("OK").clicked() {
                            self.error_msg = None;
                        }
                    });
            }

            if let Some(ref texture) = self.texture {
                ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Show the image at actual size (scrollable)
                        ui.image(texture);
                    });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.heading("Scroll in your target window to start capturing...");
                });
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Send shutdown to worker
        let _ = self.gui_tx.send(GuiCommand::Shutdown);
    }
}

/// Run the GUI application
pub fn run_gui(
    worker_rx: Receiver<WorkerEvent>,
    gui_tx: Sender<GuiCommand>,
) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Long Shot - Scrolling Screenshot")
            .with_inner_size([400.0, 600.0])
            .with_min_inner_size([300.0, 400.0])
            .with_always_on_top()
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_decorations(true)
            .with_transparent(false),
        ..Default::default()
    };

    eframe::run_native(
        "Long Shot",
        options,
        Box::new(move |cc| {
            Ok(Box::new(LongShotApp::new(cc, worker_rx, gui_tx)))
        }),
    )
}
