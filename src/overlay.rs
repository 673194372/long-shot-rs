//! Layer-shell 覆盖层窗口
//! 使用 wlr-layer-shell-unstable-v1 协议实现真正的置顶窗口

use crate::types::{CaptureRegion, GuiCommand, WorkerEvent};
use anyhow::{Context, Result};
use arboard::Clipboard;
use chrono::Local;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
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
            save_button: ButtonRect { x: 10, y: 10, width: 44, height: 44 },
            copy_button: ButtonRect { x: 64, y: 10, width: 44, height: 44 },
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
        let _status = self.status.clone();
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

        // 绘制按钮 (圆角矩形 + 图标)
        Self::draw_rounded_button(canvas, width, save_button, [45, 45, 45, 220], [80, 200, 120, 255]); // 绿色保存
        Self::draw_rounded_button(canvas, width, copy_button, [45, 45, 45, 220], [100, 180, 255, 255]); // 蓝色复制
        
        // 绘制图标
        Self::draw_save_icon(canvas, width, save_button, [255, 255, 255, 255]);
        Self::draw_copy_icon(canvas, width, copy_button, [255, 255, 255, 255]);

        // 绘制预览图像（显示底部，支持滚动）
        if let Some(ref img_data) = image_data {
            Self::draw_preview_with_scroll(canvas, width, height, img_data, image_width, image_height, scroll_offset);
        }

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

    /// 绘制保存图标 (磁盘图标)
    fn draw_save_icon(canvas: &mut [u8], width: u32, btn: ButtonRect, color: [u8; 4]) {
        let stride = width as i32 * 4;
        let cx = btn.x + btn.width / 2;
        let cy = btn.y + btn.height / 2;
        
        // 磁盘外框 (16x14)
        for dy in -7..7 {
            for dx in -8..8 {
                let px = cx + dx;
                let py = cy + dy;
                let is_border = dx == -8 || dx == 7 || dy == -7 || dy == 6;
                let is_top_notch = dy == -7 && dx > -6 && dx < 5; // 顶部缺口
                let is_label = dy >= 2 && dy <= 5 && dx >= -5 && dx <= 4; // 标签区域
                
                if (is_border && !is_top_notch) || is_label {
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
    
    /// 绘制复制图标 (剪贴板图标)
    fn draw_copy_icon(canvas: &mut [u8], width: u32, btn: ButtonRect, color: [u8; 4]) {
        let stride = width as i32 * 4;
        let cx = btn.x + btn.width / 2;
        let cy = btn.y + btn.height / 2;
        
        // 两个重叠的矩形
        // 后面的矩形
        for dy in -6..4 {
            for dx in -4..6 {
                let px = cx + dx;
                let py = cy + dy;
                let is_back = (dx == -4 || dx == 5) && dy >= -6 && dy < 1;
                let is_back_top = dy == -6 && dx >= -4 && dx <= 5;
                let is_back_bottom = dy == 0 && dx >= 2 && dx <= 5;
                
                if is_back || is_back_top || is_back_bottom {
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
        
        // 前面的矩形
        for dy in -3..7 {
            for dx in -7..3 {
                let px = cx + dx;
                let py = cy + dy;
                let is_front = dx == -7 || dx == 2 || dy == -3 || dy == 6;
                
                if is_front {
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

    /// 绘制预览图像
    /// scroll_offset: 从底部开始的偏移量（正值向上滚动查看历史）
    fn draw_preview_with_scroll(
        canvas: &mut [u8], 
        width: u32, 
        height: u32, 
        img_data: &[u8], 
        img_width: u32, 
        img_height: u32,
        scroll_offset: i32,
    ) {
        let preview_y = 60i32; // 按钮下方开始
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
    fn save_image(&self) {
        if let Some(ref data) = self.image_data {
            let timestamp = Local::now().format("%Y%m%d_%H%M%S");
            let filename = format!("longshot_{}.png", timestamp);

            let save_dir = dirs::picture_dir()
                .or_else(|| dirs::home_dir())
                .unwrap_or_else(|| PathBuf::from("."));

            let path = save_dir.join(&filename);

            match image::RgbaImage::from_raw(self.image_width, self.image_height, data.clone()) {
                Some(img) => {
                    match img.save(&path) {
                        Ok(_) => {
                            info!("图像已保存: {}", path.display());
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
        let preview_y = 60i32;
        
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    self.pointer_x = event.position.0;
                    self.pointer_y = event.position.1;
                    
                    // 处理拖动滚动
                    if self.is_dragging {
                        let delta_y = (self.pointer_y - self.drag_start_y) as i32;
                        // 向下拖动增加 offset（查看上面的内容）
                        self.scroll_offset = (self.drag_start_offset + delta_y).max(0);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if button == 272 {
                        // 左键
                        let y = self.pointer_y as i32;
                        
                        // 检查是否在预览区域内（按钮下方）
                        if y > preview_y {
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
                        // 如果不是拖动，且释放在按钮区域，才处理点击
                        if !self.is_dragging || 
                           ((self.pointer_y - self.drag_start_y).abs() < 5.0) {
                            let y = self.pointer_y as i32;
                            if y <= preview_y {
                                self.handle_click(self.pointer_x, self.pointer_y);
                            }
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
    
    // 设置窗口属性 - 固定宽度，使用屏幕高度（垂直锚定）
    layer_surface.set_size(WINDOW_WIDTH, 0); // 高度 0 表示由锚定决定
    layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT); // 垂直全屏，左侧定位
    layer_surface.set_margin(0, 0, 0, margin_left); // top, right, bottom, left
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
