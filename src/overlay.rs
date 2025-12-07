//! Layer-shell 覆盖层窗口
//! 使用 wlr-layer-shell-unstable-v1 协议实现真正的置顶窗口

use crate::types::{CaptureRegion, GuiCommand, WorkerEvent};
use anyhow::{Context, Result};
use arboard::Clipboard;
use chrono::Local;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use fontdue::{Font, FontSettings};
use log::{error, info};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::SlotPool,
        Shm, ShmHandler,
    },
};
use std::path::PathBuf;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

/// 窗口宽度
const WINDOW_WIDTH: u32 = 200;

/// 按钮区域
#[derive(Clone, Copy)]
struct ButtonRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl ButtonRect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// 覆盖层窗口状态
struct OverlayState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,

    // 窗口相关
    layer_surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    configured: bool,

    // 鼠标状态
    pointer: Option<wl_pointer::WlPointer>,
    pointer_x: f64,
    pointer_y: f64,

    // 图像数据
    image_data: Option<Vec<u8>>,
    image_width: u32,
    image_height: u32,

    // 预览滚动
    scroll_offset: i32,      // 滚动偏移（从底部开始，正值向上滚动）
    is_dragging: bool,       // 是否正在拖动
    drag_start_y: f64,       // 拖动起始 Y
    drag_start_offset: i32,  // 拖动起始偏移

    // 通信通道
    worker_rx: Receiver<WorkerEvent>,
    gui_tx: Sender<GuiCommand>,

    // 状态
    frame_count: u32,
    total_height: u32,
    status: String,
    running: bool,

    // 按钮位置
    save_button: ButtonRect,
    copy_button: ButtonRect,
    cancel_button: ButtonRect,
    
    // 选区信息
    capture_region: CaptureRegion,
    
    // 保存路径配置
    output_path: Option<std::path::PathBuf>,
    save_dir: Option<std::path::PathBuf>,
    
    // 保存后执行的命令
    exec_command: Option<String>,
    
    // 最后保存的文件路径（用于执行命令）
    last_saved_path: Option<std::path::PathBuf>,
}

impl OverlayState {
    fn new(
        registry_state: RegistryState,
        seat_state: SeatState,
        output_state: OutputState,
        compositor_state: CompositorState,
        shm_state: Shm,
        worker_rx: Receiver<WorkerEvent>,
        gui_tx: Sender<GuiCommand>,
        capture_region: CaptureRegion,
        output_path: Option<std::path::PathBuf>,
        save_dir: Option<std::path::PathBuf>,
        exec_command: Option<String>,
    ) -> Self {
        Self {
            registry_state,
            seat_state,
            output_state,
            compositor_state,
            shm_state,
            layer_surface: None,
            pool: None,
            width: WINDOW_WIDTH,
            height: 800, // 初始高度，会被 compositor 覆盖
            configured: false,
            pointer: None,
            pointer_x: 0.0,
            pointer_y: 0.0,
            image_data: None,
            image_width: 0,
            image_height: 0,
            scroll_offset: 0,
            is_dragging: false,
            drag_start_y: 0.0,
            drag_start_offset: 0,
            worker_rx,
            gui_tx,
            frame_count: 0,
            total_height: 0,
            status: "等待滚动...".to_string(),
            running: true,
            // 按钮放在底部，稍后在 draw 中根据实际高度调整
            save_button: ButtonRect { x: 5, y: 0, width: 44, height: 44 },
            copy_button: ButtonRect { x: 54, y: 0, width: 44, height: 44 },
            cancel_button: ButtonRect { x: 103, y: 0, width: 44, height: 44 },
            capture_region,
            output_path,
            save_dir,
            exec_command,
            last_saved_path: None,
        }
    }

    /// 处理来自 worker 的事件
    fn process_worker_events(&mut self) {
        loop {
            match self.worker_rx.try_recv() {
                Ok(WorkerEvent::ImageUpdated { data, width, height }) => {
                    self.image_data = Some(data);
                    self.image_width = width;
                    self.image_height = height;
                    self.frame_count += 1;
                    self.total_height = height;
                    self.status = format!("帧数: {} | 高度: {}px", self.frame_count, self.total_height);
                }
                Ok(WorkerEvent::Status(msg)) => {
                    self.status = msg;
                }
                Ok(WorkerEvent::Error(msg)) => {
                    self.status = format!("错误: {}", msg);
                    error!("Worker error: {}", msg);
                }
                Ok(WorkerEvent::CaptureComplete) => {
                    self.status = format!("捕获完成 - {} 帧, {}px", self.frame_count, self.total_height);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }
    }

    /// 绘制窗口内容
    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        if !self.configured {
            return;
        }

        // 先提取需要的值，避免借用冲突
        let width = self.width;
        let height = self.height;
        let save_button = self.save_button;
        let copy_button = self.copy_button;
        let capture_region = self.capture_region;
        let total_height = self.total_height;
        let image_data = self.image_data.clone();
        let image_width = self.image_width;
        let image_height = self.image_height;
        let scroll_offset = self.scroll_offset;

        let pool = match self.pool.as_mut() {
            Some(p) => p,
            None => return,
        };

        let stride = width as i32 * 4;

        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("创建缓冲区失败");

        // 透明背景
        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = 0;  // B
            pixel[1] = 0;  // G
            pixel[2] = 0;  // R
            pixel[3] = 0;  // A (完全透明)
        }

        // 布局：预览图占上部，按钮在底部，整体高度与选框一致
        let button_size = 44i32;
        let button_margin = 5i32;
        let preview_y = 5i32;
        let preview_height = (height as i32 - button_size - button_margin * 3 - preview_y).max(50);
        
        // 按钮位置（底部居中）- 三个按钮：保存、复制、取消
        let button_y = height as i32 - button_size - button_margin;
        let btn_gap = 8;
        let total_btn_width = button_size * 3 + btn_gap * 2; // 三个按钮 + 间距
        let btn_start_x = (width as i32 - total_btn_width) / 2;
        let save_btn = ButtonRect { x: btn_start_x, y: button_y, width: button_size, height: button_size };
        let copy_btn = ButtonRect { x: btn_start_x + button_size + btn_gap, y: button_y, width: button_size, height: button_size };
        let cancel_btn = ButtonRect { x: btn_start_x + (button_size + btn_gap) * 2, y: button_y, width: button_size, height: button_size };

        // 绘制预览图像
        if let Some(ref img_data) = image_data {
            Self::draw_preview_with_scroll_new(canvas, width, height, preview_y, preview_height as u32, img_data, image_width, image_height, scroll_offset);
        }
        
        // 绘制尺寸信息: 宽x选框高(拼接总高度)
        let stitched_height = self.image_height;
        let info_text = format!("{}x{}({})", capture_region.width, capture_region.height, stitched_height);
        
        // 先绘制半透明背景矩形，确保文本可见
        let text_bg_x = 5;
        let text_bg_y = preview_y + 3;
        let text_bg_w = 180.min(width as i32 - 10);
        let text_bg_h = 20;
        for ty in text_bg_y..(text_bg_y + text_bg_h) {
            for tx in text_bg_x..(text_bg_x + text_bg_w) {
                if tx >= 0 && ty >= 0 && tx < width as i32 && ty < height as i32 {
                    let idx = ((ty * stride) + (tx * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx] = 0;      // B
                        canvas[idx + 1] = 0;  // G
                        canvas[idx + 2] = 0;  // R
                        canvas[idx + 3] = 180; // A (半透明黑色背景)
                    }
                }
            }
        }
        
        Self::draw_info_text(canvas, width, height, 10, preview_y + 8, &info_text, [255, 255, 255, 255]);

        // 绘制图标按钮（底部，无边框）
        Self::draw_icon_button(canvas, width, save_btn, [45, 45, 45, 200], true);   // 保存图标
        Self::draw_icon_button(canvas, width, copy_btn, [45, 45, 45, 200], false);  // 复制图标
        Self::draw_cancel_button(canvas, width, cancel_btn, [80, 45, 45, 200]);     // 取消图标（红色调）
        
        // 更新按钮位置
        self.save_button = save_btn;
        self.copy_button = copy_btn;
        self.cancel_button = cancel_btn;

        // 提交并请求下一帧
        if let Some(ref layer_surface) = self.layer_surface {
            let surface = layer_surface.wl_surface();
            surface.attach(Some(buffer.wl_buffer()), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            // 请求帧回调以便持续重绘（必须在 commit 之前）
            surface.frame(_qh, surface.clone());
            surface.commit();
        }
    }

    /// 绘制保存图标 (磁盘图标，2px线宽，更清晰)
    fn draw_save_icon(canvas: &mut [u8], width: u32, btn: ButtonRect, color: [u8; 4]) {
        let stride = width as i32 * 4;
        let cx = btn.x + btn.width / 2;
        let cy = btn.y + btn.height / 2;
        let t = 2i32; // 线条粗细
        
        // 磁盘外框 (18x16)，使用填充矩形绘制粗线条
        let left = cx - 9;
        let right = cx + 9;
        let top = cy - 8;
        let bottom = cy + 8;
        
        for py in top..bottom {
            for px in left..right {
                // 外框（2px粗）
                let is_left = px >= left && px < left + t;
                let is_right = px > right - t && px < right;
                let is_top = py >= top && py < top + t && (px < cx - 4 || px >= cx + 5);
                let is_bottom = py > bottom - t && py < bottom;
                
                // 底部标签区域（实心）
                let is_label = py >= cy + 2 && py < bottom - t && px > left + t && px < right - t;
                
                if is_left || is_right || is_top || is_bottom || is_label {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() && px >= 0 && py >= 0 && px < width as i32 {
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
            }
        }
    }
    
    /// 绘制复制图标 (两个重叠矩形，2px线宽，更清晰)
    fn draw_copy_icon(canvas: &mut [u8], width: u32, btn: ButtonRect, color: [u8; 4]) {
        let stride = width as i32 * 4;
        let cx = btn.x + btn.width / 2;
        let cy = btn.y + btn.height / 2;
        let t = 2i32; // 线条粗细
        
        // 后面的矩形（右上）
        let b_left = cx - 2;
        let b_right = cx + 8;
        let b_top = cy - 8;
        let b_bottom = cy + 2;
        
        for py in b_top..b_bottom {
            for px in b_left..b_right {
                let is_left = px >= b_left && px < b_left + t && py < cy - 2;
                let is_right = px > b_right - t && px < b_right;
                let is_top = py >= b_top && py < b_top + t;
                let is_bottom = py > b_bottom - t && py < b_bottom && px > cx + 2;
                
                if is_left || is_right || is_top || is_bottom {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() && px >= 0 && py >= 0 && px < width as i32 {
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
            }
        }
        
        // 前面的矩形（左下）
        let f_left = cx - 8;
        let f_right = cx + 2;
        let f_top = cy - 2;
        let f_bottom = cy + 8;
        
        for py in f_top..f_bottom {
            for px in f_left..f_right {
                let is_left = px >= f_left && px < f_left + t;
                let is_right = px > f_right - t && px < f_right;
                let is_top = py >= f_top && py < f_top + t;
                let is_bottom = py > f_bottom - t && py < f_bottom;
                
                if is_left || is_right || is_top || is_bottom {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() && px >= 0 && py >= 0 && px < width as i32 {
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
            }
        }
    }

    /// 绘制取消按钮 (X 图标，抗锯齿)
    fn draw_cancel_button(canvas: &mut [u8], canvas_width: u32, btn: ButtonRect, bg_color: [u8; 4]) {
        let stride = canvas_width as i32 * 4;
        let r = 6.0f32; // 圆角半径
        
        // 绘制圆角矩形背景（使用浮点计算实现平滑边缘）
        for dy in 0..btn.height {
            for dx in 0..btn.width {
                let px = btn.x + dx;
                let py = btn.y + dy;
                
                if px < 0 || py < 0 || px >= canvas_width as i32 {
                    continue;
                }
                
                // 计算到最近圆角中心的距离
                let fx = dx as f32 + 0.5;
                let fy = dy as f32 + 0.5;
                let w = btn.width as f32;
                let h = btn.height as f32;
                
                // 确定是否在圆角区域
                let in_corner_region = (fx < r && fy < r) || 
                                       (fx > w - r && fy < r) ||
                                       (fx < r && fy > h - r) ||
                                       (fx > w - r && fy > h - r);
                
                let alpha = if in_corner_region {
                    // 计算到圆角中心的距离
                    let (cx, cy) = if fx < r && fy < r {
                        (r, r)
                    } else if fx > w - r && fy < r {
                        (w - r, r)
                    } else if fx < r && fy > h - r {
                        (r, h - r)
                    } else {
                        (w - r, h - r)
                    };
                    
                    let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                    if dist <= r - 0.5 {
                        1.0
                    } else if dist >= r + 0.5 {
                        0.0
                    } else {
                        // 抗锯齿：平滑过渡
                        r + 0.5 - dist
                    }
                } else {
                    1.0
                };
                
                if alpha > 0.0 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        if alpha >= 1.0 {
                            canvas[idx..idx+4].copy_from_slice(&bg_color);
                        } else {
                            // Alpha 混合
                            let a = (alpha * bg_color[3] as f32) as u8;
                            canvas[idx] = bg_color[0];
                            canvas[idx + 1] = bg_color[1];
                            canvas[idx + 2] = bg_color[2];
                            canvas[idx + 3] = a;
                        }
                    }
                }
            }
        }
        
        // 绘制 X 图标 (两条对角线，2px粗)
        let color: [u8; 4] = [255, 100, 100, 255]; // 红色调
        let cx = btn.x + btn.width / 2;
        let cy = btn.y + btn.height / 2;
        let size = 8i32;
        
        // 绘制粗线条的 X
        for i in -size..=size {
            for t in -1..=1 {
                // 左上到右下的对角线
                let px1 = cx + i;
                let py1 = cy + i + t;
                if px1 >= 0 && py1 >= 0 && px1 < canvas_width as i32 {
                    let idx = ((py1 * stride) + (px1 * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
                
                // 右上到左下的对角线
                let px2 = cx + i;
                let py2 = cy - i + t;
                if px2 >= 0 && py2 >= 0 && px2 < canvas_width as i32 {
                    let idx = ((py2 * stride) + (px2 * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
            }
        }
    }

    /// 绘制文本区域 (静态方法)
    fn draw_text_static(canvas: &mut [u8], width: u32, height: u32, x: i32, y: i32, _text: &str, color: [u8; 4]) {
        let stride = width as i32 * 4;
        for dy in 0..12 {
            for dx in 0..150.min(width as i32 - x) {
                let px = x + dx;
                let py = y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx] = color[0];
                        canvas[idx + 1] = color[1];
                        canvas[idx + 2] = color[2];
                        canvas[idx + 3] = color[3];
                    }
                }
            }
        }
    }
    
    /// 加载系统字体
    fn load_font() -> Option<Font> {
        let font_paths = [
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        ];
        
        for path in &font_paths {
            if let Ok(data) = std::fs::read(path) {
                if let Ok(font) = Font::from_bytes(data, FontSettings::default()) {
                    return Some(font);
                }
            }
        }
        None
    }
    
    /// 绘制信息文本
    fn draw_info_text(canvas: &mut [u8], width: u32, height: u32, x: i32, y: i32, text: &str, color: [u8; 4]) {
        let stride = width as i32 * 4;
        
        if let Some(font) = Self::load_font() {
            // 使用字体渲染
            let font_size = 12.0f32;
            let mut cursor_x = x;
            
            for ch in text.chars() {
                let (metrics, bitmap) = font.rasterize(ch, font_size);
                let char_x = cursor_x + metrics.xmin;
                let char_y = y + (font_size as i32 - metrics.height as i32 - metrics.ymin);
                
                for row in 0..metrics.height {
                    for col in 0..metrics.width {
                        let alpha = bitmap[row * metrics.width + col];
                        if alpha > 0 {
                            let px = char_x + col as i32;
                            let py = char_y + row as i32;
                            
                            if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                                let idx = ((py * stride) + (px * 4)) as usize;
                                if idx + 3 < canvas.len() {
                                    let a = alpha as u32;
                                    let inv_a = 255 - a;
                                    canvas[idx] = ((color[0] as u32 * a + canvas[idx] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 1] = ((color[1] as u32 * a + canvas[idx + 1] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 2] = ((color[2] as u32 * a + canvas[idx + 2] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 3] = 255;
                                }
                            }
                        }
                    }
                }
                cursor_x += metrics.advance_width as i32;
            }
        } else {
            // Fallback: 简单像素字符
            let mut char_x = x;
            for ch in text.chars() {
                Self::draw_char_fallback(canvas, stride, width, height, char_x, y, ch, color);
                char_x += 8;
            }
        }
    }
    
    /// Fallback 字符绘制
    fn draw_char_fallback(canvas: &mut [u8], stride: i32, width: u32, height: u32, x: i32, y: i32, ch: char, color: [u8; 4]) {
        let patterns: u8 = match ch {
            '0' => 0b1111110, '1' => 0b0110000, '2' => 0b1101101,
            '3' => 0b1111001, '4' => 0b0110011, '5' => 0b1011011,
            '6' => 0b1011111, '7' => 0b1110000, '8' => 0b1111111,
            '9' => 0b1111011, _ => 0,
        };
        
        for dy in 0..10i32 {
            for dx in 0..6i32 {
                let px = x + dx;
                let py = y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let should_draw = match ch {
                        'x' => (dx == 1 && dy == 3) || (dx == 4 && dy == 3) ||
                               (dx == 2 && dy == 4) || (dx == 3 && dy == 4) ||
                               (dx == 2 && dy == 5) || (dx == 3 && dy == 5) ||
                               (dx == 1 && dy == 6) || (dx == 4 && dy == 6),
                        '0'..='9' => {
                            let p = patterns;
                            match (dx, dy) {
                                (1..=4, 0) if p & 0b1000000 != 0 => true,
                                (0, 1..=4) if p & 0b0100000 != 0 => true,
                                (5, 1..=4) if p & 0b0010000 != 0 => true,
                                (1..=4, 4) if p & 0b0001000 != 0 => true,
                                (0, 5..=8) if p & 0b0000100 != 0 => true,
                                (5, 5..=8) if p & 0b0000010 != 0 => true,
                                (1..=4, 9) if p & 0b0000001 != 0 => true,
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    
                    if should_draw {
                        let idx = ((py * stride) + (px * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx..idx+4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
    }

    /// 绘制图标按钮（无边框，圆角背景 + 图标，抗锯齿）
    fn draw_icon_button(canvas: &mut [u8], canvas_width: u32, btn: ButtonRect, bg_color: [u8; 4], is_save: bool) {
        let stride = canvas_width as i32 * 4;
        let r = 6.0f32; // 圆角半径
        
        // 绘制圆角矩形背景（使用浮点计算实现平滑边缘）
        for dy in 0..btn.height {
            for dx in 0..btn.width {
                let px = btn.x + dx;
                let py = btn.y + dy;
                
                if px < 0 || py < 0 || px >= canvas_width as i32 {
                    continue;
                }
                
                // 计算到最近圆角中心的距离
                let fx = dx as f32 + 0.5;
                let fy = dy as f32 + 0.5;
                let w = btn.width as f32;
                let h = btn.height as f32;
                
                // 确定是否在圆角区域
                let in_corner_region = (fx < r && fy < r) || 
                                       (fx > w - r && fy < r) ||
                                       (fx < r && fy > h - r) ||
                                       (fx > w - r && fy > h - r);
                
                let alpha = if in_corner_region {
                    // 计算到圆角中心的距离
                    let (cx, cy) = if fx < r && fy < r {
                        (r, r)
                    } else if fx > w - r && fy < r {
                        (w - r, r)
                    } else if fx < r && fy > h - r {
                        (r, h - r)
                    } else {
                        (w - r, h - r)
                    };
                    
                    let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                    if dist <= r - 0.5 {
                        1.0
                    } else if dist >= r + 0.5 {
                        0.0
                    } else {
                        // 抗锯齿：平滑过渡
                        r + 0.5 - dist
                    }
                } else {
                    1.0
                };
                
                if alpha > 0.0 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        if alpha >= 1.0 {
                            canvas[idx..idx+4].copy_from_slice(&bg_color);
                        } else {
                            // Alpha 混合
                            let a = (alpha * bg_color[3] as f32) as u8;
                            canvas[idx] = bg_color[0];
                            canvas[idx + 1] = bg_color[1];
                            canvas[idx + 2] = bg_color[2];
                            canvas[idx + 3] = a;
                        }
                    }
                }
            }
        }
        
        // 绘制图标
        let icon_color: [u8; 4] = [255, 255, 255, 255];
        if is_save {
            Self::draw_save_icon(canvas, canvas_width, btn, icon_color);
        } else {
            Self::draw_copy_icon(canvas, canvas_width, btn, icon_color);
        }
    }
    
    /// 绘制带文字的按钮（保留备用）
    #[allow(dead_code)]
    fn draw_button_with_text(
        canvas: &mut [u8], 
        canvas_width: u32, 
        canvas_height: u32,
        btn: ButtonRect, 
        text: &str,
        bg_color: [u8; 4], 
        border_color: [u8; 4],
    ) {
        let stride = canvas_width as i32 * 4;
        let radius = 6i32;
        
        // 绘制圆角矩形背景
        for dy in 0..btn.height {
            for dx in 0..btn.width {
                let px = btn.x + dx;
                let py = btn.y + dy;
                
                if px < 0 || py < 0 || px >= canvas_width as i32 || py >= canvas_height as i32 {
                    continue;
                }
                
                // 圆角检测
                let in_corner = |cx: i32, cy: i32| -> bool {
                    let ddx = (px - cx).abs();
                    let ddy = (py - cy).abs();
                    ddx * ddx + ddy * ddy > radius * radius
                };
                
                let skip = (dx < radius && dy < radius && in_corner(btn.x + radius, btn.y + radius)) ||
                           (dx >= btn.width - radius && dy < radius && in_corner(btn.x + btn.width - radius - 1, btn.y + radius)) ||
                           (dx < radius && dy >= btn.height - radius && in_corner(btn.x + radius, btn.y + btn.height - radius - 1)) ||
                           (dx >= btn.width - radius && dy >= btn.height - radius && in_corner(btn.x + btn.width - radius - 1, btn.y + btn.height - radius - 1));
                
                if !skip {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        // 边框或背景
                        let is_border = dx < 2 || dx >= btn.width - 2 || dy < 2 || dy >= btn.height - 2;
                        let color = if is_border { border_color } else { bg_color };
                        canvas[idx..idx+4].copy_from_slice(&color);
                    }
                }
            }
        }
        
        // 绘制文字（居中）
        if let Some(font) = Self::load_font() {
            let font_size = 14.0f32;
            // 计算文字宽度
            let mut text_width = 0i32;
            for ch in text.chars() {
                let (metrics, _) = font.rasterize(ch, font_size);
                text_width += metrics.advance_width as i32;
            }
            
            let text_x = btn.x + (btn.width - text_width) / 2;
            let text_y = btn.y + (btn.height - font_size as i32) / 2 + 2;
            
            let mut cursor_x = text_x;
            for ch in text.chars() {
                let (metrics, bitmap) = font.rasterize(ch, font_size);
                let char_x = cursor_x + metrics.xmin;
                let char_y = text_y + (font_size as i32 - metrics.height as i32 - metrics.ymin);
                
                for row in 0..metrics.height {
                    for col in 0..metrics.width {
                        let alpha = bitmap[row * metrics.width + col];
                        if alpha > 0 {
                            let px = char_x + col as i32;
                            let py = char_y + row as i32;
                            
                            if px >= 0 && py >= 0 && px < canvas_width as i32 && py < canvas_height as i32 {
                                let idx = ((py * stride) + (px * 4)) as usize;
                                if idx + 3 < canvas.len() {
                                    let a = alpha as u32;
                                    let inv_a = 255 - a;
                                    canvas[idx] = ((255u32 * a + canvas[idx] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 1] = ((255u32 * a + canvas[idx + 1] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 2] = ((255u32 * a + canvas[idx + 2] as u32 * inv_a) / 255) as u8;
                                    canvas[idx + 3] = 255;
                                }
                            }
                        }
                    }
                }
                cursor_x += metrics.advance_width as i32;
            }
        }
    }

    /// 绘制预览图像（新布局版本）
    fn draw_preview_with_scroll_new(
        canvas: &mut [u8], 
        canvas_width: u32, 
        _canvas_height: u32,
        preview_y: i32,
        preview_height: u32,
        img_data: &[u8], 
        img_width: u32, 
        img_height: u32,
        scroll_offset: i32,
    ) {
        let preview_width = (canvas_width - 10) as u32;

        if img_width == 0 || img_height == 0 || preview_width == 0 || preview_height == 0 {
            return;
        }

        // 固定宽度显示，计算缩放比例
        let scale = preview_width as f32 / img_width as f32;
        let scaled_img_height = (img_height as f32 * scale) as i32;
        
        let max_scroll = (scaled_img_height - preview_height as i32).max(0);
        let clamped_offset = scroll_offset.clamp(0, max_scroll);
        
        let view_bottom = scaled_img_height - clamped_offset;
        let view_top = (view_bottom - preview_height as i32).max(0);
        
        let offset_x = 5i32;
        let stride = canvas_width as i32 * 4;
        let src_stride = img_width as usize * 4;
        let inv_scale = 1.0 / scale;

        for dy in 0..preview_height {
            let scaled_y = view_top + dy as i32;
            if scaled_y < 0 || scaled_y >= scaled_img_height {
                continue;
            }
            
            for dx in 0..preview_width {
                let src_x_start = (dx as f32 * inv_scale) as usize;
                let src_y_start = (scaled_y as f32 * inv_scale) as usize;
                let src_x_end = ((dx + 1) as f32 * inv_scale).ceil() as usize;
                let src_y_end = ((scaled_y + 1) as f32 * inv_scale).ceil() as usize;
                
                let src_x_end = src_x_end.min(img_width as usize);
                let src_y_end = src_y_end.min(img_height as usize);
                
                let mut r_sum = 0u32;
                let mut g_sum = 0u32;
                let mut b_sum = 0u32;
                let mut count = 0u32;
                
                for sy in src_y_start..src_y_end {
                    for sx in src_x_start..src_x_end {
                        let src_idx = sy * src_stride + sx * 4;
                        if src_idx + 3 < img_data.len() {
                            r_sum += img_data[src_idx] as u32;
                            g_sum += img_data[src_idx + 1] as u32;
                            b_sum += img_data[src_idx + 2] as u32;
                            count += 1;
                        }
                    }
                }
                
                if count > 0 {
                    let dst_x = offset_x + dx as i32;
                    let dst_y = preview_y + dy as i32;
                    
                    if dst_x >= 0 && dst_y >= 0 && dst_x < canvas_width as i32 {
                        let dst_idx = (dst_y * stride + dst_x * 4) as usize;
                        if dst_idx + 3 < canvas.len() {
                            canvas[dst_idx] = (b_sum / count) as u8;
                            canvas[dst_idx + 1] = (g_sum / count) as u8;
                            canvas[dst_idx + 2] = (r_sum / count) as u8;
                            canvas[dst_idx + 3] = 255;
                        }
                    }
                }
            }
        }
        
        // 红色边框 (1px)
        let border_color: [u8; 4] = [0, 0, 255, 255];
        let bw = preview_width as i32;
        let bh = preview_height as i32;
        
        // 上下边
        for dx in 0..bw {
            let px = offset_x + dx;
            for &py in &[preview_y, preview_y + bh - 1] {
                if px >= 0 && py >= 0 && px < canvas_width as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
        }
        // 左右边
        for dy in 0..bh {
            let py = preview_y + dy;
            for &px in &[offset_x, offset_x + bw - 1] {
                if px >= 0 && py >= 0 && px < canvas_width as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
        }
    }

    /// 绘制预览图像（旧版本，保留兼容）
    #[allow(dead_code)]
    fn draw_preview_with_scroll(
        canvas: &mut [u8], 
        width: u32, 
        height: u32, 
        img_data: &[u8], 
        img_width: u32, 
        img_height: u32,
        scroll_offset: i32,
    ) {
        let preview_y = 60i32;
        let preview_height = (height as i32 - preview_y - 5).max(0) as u32;
        let preview_width = (width - 10) as u32;

        if img_width == 0 || img_height == 0 || preview_width == 0 || preview_height == 0 {
            return;
        }

        // 固定宽度显示，计算缩放比例
        let scale = preview_width as f32 / img_width as f32;
        let scaled_img_height = (img_height as f32 * scale) as i32;
        
        // 计算源图像的起始 Y（从底部开始，加上滚动偏移）
        // scroll_offset = 0 时显示底部，scroll_offset > 0 时向上滚动
        let max_scroll = (scaled_img_height - preview_height as i32).max(0);
        let clamped_offset = scroll_offset.clamp(0, max_scroll);
        
        // 在缩放后的图像中，显示的区域
        let view_bottom = scaled_img_height - clamped_offset;
        let view_top = (view_bottom - preview_height as i32).max(0);
        
        let offset_x = 5i32;
        let offset_y = preview_y;

        let stride = width as i32 * 4;
        let src_stride = img_width as usize * 4;
        let inv_scale = 1.0 / scale;

        // 绘制预览区域
        for dy in 0..preview_height {
            // 缩放后图像中的 Y 坐标
            let scaled_y = view_top + dy as i32;
            if scaled_y < 0 || scaled_y >= scaled_img_height {
                continue;
            }
            
            for dx in 0..preview_width {
                // 计算源图像对应区域（Box filter）
                let src_x_start = (dx as f32 * inv_scale) as usize;
                let src_y_start = (scaled_y as f32 * inv_scale) as usize;
                let src_x_end = ((dx + 1) as f32 * inv_scale).ceil() as usize;
                let src_y_end = ((scaled_y + 1) as f32 * inv_scale).ceil() as usize;
                
                let src_x_end = src_x_end.min(img_width as usize);
                let src_y_end = src_y_end.min(img_height as usize);
                
                // 区域内像素求平均
                let mut r_sum = 0u32;
                let mut g_sum = 0u32;
                let mut b_sum = 0u32;
                let mut count = 0u32;
                
                for sy in src_y_start..src_y_end {
                    for sx in src_x_start..src_x_end {
                        let src_idx = sy * src_stride + sx * 4;
                        if src_idx + 3 < img_data.len() {
                            r_sum += img_data[src_idx] as u32;
                            g_sum += img_data[src_idx + 1] as u32;
                            b_sum += img_data[src_idx + 2] as u32;
                            count += 1;
                        }
                    }
                }
                
                if count > 0 {
                    let dst_x = offset_x + dx as i32;
                    let dst_y = offset_y + dy as i32;
                    
                    if dst_x >= 0 && dst_y >= 0 && dst_x < width as i32 && dst_y < height as i32 {
                        let dst_idx = (dst_y * stride + dst_x * 4) as usize;
                        if dst_idx + 3 < canvas.len() {
                            canvas[dst_idx] = (b_sum / count) as u8;     // B
                            canvas[dst_idx + 1] = (g_sum / count) as u8; // G
                            canvas[dst_idx + 2] = (r_sum / count) as u8; // R
                            canvas[dst_idx + 3] = 255;                   // A
                        }
                    }
                }
            }
        }
        
        // 绘制红色边框 (2px)
        let border_color: [u8; 4] = [0, 0, 255, 255]; // BGRA: 红色
        let display_height = preview_height.min(scaled_img_height as u32);
        for border in 0..2i32 {
            // 上边
            for dx in 0..preview_width as i32 {
                let px = offset_x + dx;
                let py = offset_y + border;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 下边
            for dx in 0..preview_width as i32 {
                let px = offset_x + dx;
                let py = offset_y + display_height as i32 - 1 - border;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 左边
            for dy in 0..display_height as i32 {
                let px = offset_x + border;
                let py = offset_y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 右边
            for dy in 0..display_height as i32 {
                let px = offset_x + preview_width as i32 - 1 - border;
                let py = offset_y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
        }
    }

    /// 保存图像
    fn save_image(&mut self) {
        if let Some(ref data) = self.image_data {
            // 确定保存路径
            let path = if let Some(ref output) = self.output_path {
                // 使用命令行指定的输出路径
                output.clone()
            } else {
                // 生成默认文件名
                let timestamp = Local::now().format("%Y%m%d_%H%M%S");
                let filename = format!("longshot_{}.png", timestamp);
                
                // 确定保存目录
                let save_dir = if let Some(ref dir) = self.save_dir {
                    dir.clone()
                } else {
                    dirs::picture_dir()
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(|| PathBuf::from("."))
                };
                
                save_dir.join(&filename)
            };

            match image::RgbaImage::from_raw(self.image_width, self.image_height, data.clone()) {
                Some(img) => {
                    // 确保父目录存在
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    
                    match img.save(&path) {
                        Ok(_) => {
                            info!("图像已保存: {}", path.display());
                            println!("✅ 图像已保存: {}", path.display());
                            
                            // 保存路径用于后续执行命令
                            self.last_saved_path = Some(path.clone());
                            
                            // 执行后续命令
                            if let Some(ref cmd) = self.exec_command {
                                self.execute_command(cmd, &path);
                            }
                        }
                        Err(e) => {
                            error!("保存失败: {}", e);
                        }
                    }
                }
                None => {
                    error!("无法创建图像缓冲区");
                }
            }
        }
    }
    
    /// 执行后续命令
    fn execute_command(&self, cmd_template: &str, file_path: &std::path::Path) {
        let path_str = file_path.display().to_string();
        let cmd = cmd_template.replace("{}", &path_str);
        
        info!("执行命令: {}", cmd);
        
        // 使用 sh -c 执行命令
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .spawn()
        {
            Ok(_) => {
                info!("命令已启动");
            }
            Err(e) => {
                error!("执行命令失败: {}", e);
            }
        }
    }

    /// 复制到剪贴板
    fn copy_to_clipboard(&self) {
        if let Some(ref data) = self.image_data {
            match Clipboard::new() {
                Ok(mut clipboard) => {
                    let img = arboard::ImageData {
                        width: self.image_width as usize,
                        height: self.image_height as usize,
                        bytes: std::borrow::Cow::Borrowed(data),
                    };
                    match clipboard.set_image(img) {
                        Ok(_) => {
                            info!("图像已复制到剪贴板");
                        }
                        Err(e) => {
                            error!("复制失败: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("无法访问剪贴板: {}", e);
                }
            }
        }
    }

    /// 处理点击
    fn handle_click(&mut self, x: f64, y: f64) {
        let ix = x as i32;
        let iy = y as i32;

        if self.save_button.contains(ix, iy) {
            info!("点击保存按钮");
            self.save_image();
            self.running = false; // 保存后退出
        } else if self.copy_button.contains(ix, iy) {
            info!("点击复制按钮");
            self.copy_to_clipboard();
            self.running = false; // 复制后退出
        } else if self.cancel_button.contains(ix, iy) {
            info!("点击取消按钮");
            self.running = false; // 直接退出，不保存
        }
    }
    
    /// 绘制圆角按钮
    fn draw_rounded_button(canvas: &mut [u8], width: u32, btn: ButtonRect, bg_color: [u8; 4], accent_color: [u8; 4]) {
        let stride = width as i32 * 4;
        let radius = 8i32;
        
        for dy in 0..btn.height {
            for dx in 0..btn.width {
                let px = btn.x + dx;
                let py = btn.y + dy;
                
                // 检查是否在圆角范围内
                let in_corner = |cx: i32, cy: i32| -> bool {
                    let dist_sq = (dx - cx) * (dx - cx) + (dy - cy) * (dy - cy);
                    dist_sq <= radius * radius
                };
                
                let is_inside = 
                    // 非角落区域
                    (dx >= radius && dx < btn.width - radius) ||
                    (dy >= radius && dy < btn.height - radius) ||
                    // 四个角落的圆形检查
                    in_corner(radius, radius) ||
                    in_corner(btn.width - radius - 1, radius) ||
                    in_corner(radius, btn.height - radius - 1) ||
                    in_corner(btn.width - radius - 1, btn.height - radius - 1);
                
                if is_inside {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        // 顶部用强调色条
                        if dy < 3 {
                            canvas[idx] = accent_color[0];
                            canvas[idx + 1] = accent_color[1];
                            canvas[idx + 2] = accent_color[2];
                            canvas[idx + 3] = accent_color[3];
                        } else {
                            canvas[idx] = bg_color[0];
                            canvas[idx + 1] = bg_color[1];
                            canvas[idx + 2] = bg_color[2];
                            canvas[idx + 3] = bg_color[3];
                        }
                    }
                }
            }
        }
    }
}

// 实现各种 Handler traits
impl CompositorHandler for OverlayState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.process_worker_events();
        self.draw(qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for OverlayState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for OverlayState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.running = false;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // 接受 compositor 发送的尺寸
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }

        self.configured = true;

        // 创建 SHM pool
        if self.pool.is_none() {
            let size = (self.width * self.height * 4) as usize;
            self.pool = Some(
                SlotPool::new(size * 2, &self.shm_state)
                    .expect("创建 SHM pool 失败"),
            );
        }

        self.draw(qh);
    }
}

impl SeatHandler for OverlayState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat).ok();
            self.pointer = pointer;
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl PointerHandler for OverlayState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // 新布局：预览在上，按钮在下
        let button_area_y = self.save_button.y; // 按钮区域起始 Y
        
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    self.pointer_x = event.position.0;
                    self.pointer_y = event.position.1;
                    
                    // 处理拖动滚动
                    if self.is_dragging {
                        let delta_y = (self.pointer_y - self.drag_start_y) as i32;
                        self.scroll_offset = (self.drag_start_offset + delta_y).max(0);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if button == 272 {
                        let y = self.pointer_y as i32;
                        
                        // 检查是否在预览区域内（按钮区域上方）
                        if y < button_area_y {
                            // 在预览区域开始拖动
                            self.is_dragging = true;
                            self.drag_start_y = self.pointer_y;
                            self.drag_start_offset = self.scroll_offset;
                        } else {
                            // 在按钮区域，处理点击
                            self.handle_click(self.pointer_x, self.pointer_y);
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == 272 {
                        // 如果移动很小，当作点击处理
                        if self.is_dragging && (self.pointer_y - self.drag_start_y).abs() < 5.0 {
                            // 在预览区域的点击不做处理
                        } else if !self.is_dragging {
                            // 按钮区域点击
                            self.handle_click(self.pointer_x, self.pointer_y);
                        }
                        self.is_dragging = false;
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for OverlayState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ProvidesRegistryState for OverlayState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(OverlayState);
delegate_output!(OverlayState);
delegate_shm!(OverlayState);
delegate_seat!(OverlayState);
delegate_pointer!(OverlayState);
delegate_layer!(OverlayState);
delegate_registry!(OverlayState);

/// 运行 Layer Shell 覆盖层窗口
pub fn run_overlay(
    worker_rx: Receiver<WorkerEvent>,
    gui_tx: Sender<GuiCommand>,
    region: CaptureRegion,
    output_path: Option<std::path::PathBuf>,
    save_dir: Option<std::path::PathBuf>,
    exec_command: Option<String>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("无法连接到 Wayland")?;

    let (globals, mut event_queue) = registry_queue_init(&conn).context("初始化注册表失败")?;
    let qh = event_queue.handle();

    // 初始化各个状态
    let compositor_state = CompositorState::bind(&globals, &qh).context("绑定 compositor 失败")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("绑定 layer-shell 失败")?;
    let shm_state = Shm::bind(&globals, &qh).context("绑定 shm 失败")?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    let mut state = OverlayState::new(
        registry_state,
        seat_state,
        output_state,
        compositor_state,
        shm_state,
        worker_rx,
        gui_tx.clone(),
        region,
        output_path,
        save_dir,
        exec_command,
    );

    // 创建 layer surface
    let surface = state.compositor_state.create_surface(&qh);

    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay, // 使用 Overlay 层，确保在最顶层
        Some("long-shot-preview"),
        None, // 所有输出
    );

    // 计算窗口位置：选框右侧，保持 10px 间距
    let margin_left = region.x + region.width as i32 + 10;
    let margin_top = region.y; // 与选框顶部对齐
    
    // 设置窗口属性 - 固定宽度，高度与选框一致
    layer_surface.set_size(WINDOW_WIDTH, region.height);
    layer_surface.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer_surface.set_margin(margin_top, 0, 0, margin_left);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.commit();

    state.layer_surface = Some(layer_surface);

    info!("Layer Shell 覆盖层窗口已创建 (选框右侧 x={})", margin_left);

    // 主事件循环
    while state.running {
        event_queue.blocking_dispatch(&mut state)?;
    }

    // 发送关闭信号
    let _ = gui_tx.send(GuiCommand::Shutdown);

    Ok(())
}
