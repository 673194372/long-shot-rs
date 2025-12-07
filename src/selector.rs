//! 选区选择器和边框显示层
//! 实现自定义选框替代 slurp，选区后常驻显示边框

use crate::types::CaptureRegion;
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use log::info;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_compositor::WlCompositor, wl_output, wl_pointer, wl_region::WlRegion, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

// ============================================================================
// 选区选择器 (SelectorState)
// ============================================================================

/// 选区选择器状态
struct SelectorState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,

    layer_surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    configured: bool,

    pointer: Option<wl_pointer::WlPointer>,
    
    // 选区状态
    start_x: i32,
    start_y: i32,
    current_x: i32,
    current_y: i32,
    is_selecting: bool,
    selection_complete: bool,
    
    running: bool,
    first_draw: bool,       // 首次绘制标志
    needs_redraw: bool,     // 需要重绘标志
    last_draw_time: Instant, // 上次绘制时间
}

impl SelectorState {
    fn new(
        registry_state: RegistryState,
        seat_state: SeatState,
        output_state: OutputState,
        compositor_state: CompositorState,
        shm_state: Shm,
    ) -> Self {
        Self {
            registry_state,
            seat_state,
            output_state,
            compositor_state,
            shm_state,
            layer_surface: None,
            pool: None,
            width: 1920,
            height: 1080,
            configured: false,
            pointer: None,
            start_x: 0,
            start_y: 0,
            current_x: 0,
            current_y: 0,
            is_selecting: false,
            selection_complete: false,
            running: true,
            first_draw: true,
            needs_redraw: false,
            last_draw_time: Instant::now(),
        }
    }

    fn get_region(&self) -> CaptureRegion {
        let x = self.start_x.min(self.current_x);
        let y = self.start_y.min(self.current_y);
        let w = (self.start_x - self.current_x).unsigned_abs();
        let h = (self.start_y - self.current_y).unsigned_abs();
        CaptureRegion {
            x,
            y,
            width: w.max(1),
            height: h.max(1),
        }
    }

    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        if !self.configured {
            return;
        }

        // 限制重绘频率：最多 30 FPS（约 33ms）
        let now = Instant::now();
        if !self.first_draw && now.duration_since(self.last_draw_time).as_millis() < 33 {
            return;
        }
        self.last_draw_time = now;

        let width = self.width;
        let height = self.height;
        let is_selecting = self.is_selecting;
        let region = self.get_region();

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

        // 半透明黑色背景
        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = 0;   // B
            pixel[1] = 0;   // G
            pixel[2] = 0;   // R
            pixel[3] = 120; // A (半透明)
        }

        // 如果正在选择，绘制选区
        if is_selecting && region.width > 1 && region.height > 1 {
            // 选区内部透明（挖空）
            for y in region.y..(region.y + region.height as i32) {
                for x in region.x..(region.x + region.width as i32) {
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        let idx = ((y * stride) + (x * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx] = 0;     // B
                            canvas[idx + 1] = 0; // G
                            canvas[idx + 2] = 0; // R
                            canvas[idx + 3] = 0; // A (透明)
                        }
                    }
                }
            }
            
            // 红色边框 (2px)
            let border_color: [u8; 4] = [0, 0, 255, 255]; // BGRA: 红色
            for border in 0..2i32 {
                // 上边
                for x in region.x..(region.x + region.width as i32) {
                    let y = region.y + border;
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        let idx = ((y * stride) + (x * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx..idx+4].copy_from_slice(&border_color);
                        }
                    }
                }
                // 下边
                for x in region.x..(region.x + region.width as i32) {
                    let y = region.y + region.height as i32 - 1 - border;
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        let idx = ((y * stride) + (x * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx..idx+4].copy_from_slice(&border_color);
                        }
                    }
                }
                // 左边
                for y in region.y..(region.y + region.height as i32) {
                    let x = region.x + border;
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        let idx = ((y * stride) + (x * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx..idx+4].copy_from_slice(&border_color);
                        }
                    }
                }
                // 右边
                for y in region.y..(region.y + region.height as i32) {
                    let x = region.x + region.width as i32 - 1 - border;
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        let idx = ((y * stride) + (x * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            canvas[idx..idx+4].copy_from_slice(&border_color);
                        }
                    }
                }
            }
            
            // 显示尺寸
            Self::draw_size_label_selector(canvas, stride, width, height, &region);
        }

        // 提交（不请求 frame callback，避免持续重绘闪烁）
        if let Some(ref layer_surface) = self.layer_surface {
            let surface = layer_surface.wl_surface();
            surface.attach(Some(buffer.wl_buffer()), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.commit();
        }
        self.first_draw = false;
    }
    
    fn draw_size_label_selector(canvas: &mut [u8], stride: i32, width: u32, height: u32, region: &CaptureRegion) {
        let label_x = region.x;
        let label_y = (region.y - 24).max(0);
        let text = format!("{}x{}", region.width, region.height);
        let label_w = (text.len() * 8 + 12) as i32;
        let label_h = 20i32;
        
        // 背景
        for dy in 0..label_h {
            for dx in 0..label_w {
                let px = label_x + dx;
                let py = label_y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx] = 40;      // B
                        canvas[idx + 1] = 40;  // G
                        canvas[idx + 2] = 40;  // R
                        canvas[idx + 3] = 230; // A
                    }
                }
            }
        }
        
        // 使用字体渲染
        draw_text(canvas, stride, width, height, label_x + 4, label_y + 3, &text, 14.0, [255, 255, 255, 255]);
    }
}

/// 统一的文本绘制函数（自动选择字体或 fallback）
fn draw_text(
    canvas: &mut [u8], 
    stride: i32, 
    width: u32, 
    height: u32, 
    x: i32, 
    y: i32, 
    text: &str,
    font_size: f32,
    color: [u8; 4],
) {
    // 尝试加载字体
    if let Some(font) = load_font() {
        draw_text_with_font(canvas, stride, width, height, x, y, text, &font, font_size, color);
    } else {
        // Fallback 到简单字符
        draw_text_fallback(canvas, stride, width, height, x, y, text, color);
    }
}

/// 加载系统字体
fn load_font() -> Option<Font> {
    // 尝试常见的系统字体路径
    let font_paths = [
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
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

/// 使用字体渲染文本
fn draw_text_with_font(
    canvas: &mut [u8], 
    stride: i32, 
    canvas_width: u32, 
    canvas_height: u32, 
    x: i32, 
    y: i32, 
    text: &str,
    font: &Font,
    font_size: f32,
    color: [u8; 4],
) {
    let mut cursor_x = x;
    
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        
        // 计算字符位置
        let char_x = cursor_x + metrics.xmin;
        let char_y = y + (font_size as i32 - metrics.height as i32 - metrics.ymin);
        
        // 绘制字符
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let alpha = bitmap[row * metrics.width + col];
                if alpha > 0 {
                    let px = char_x + col as i32;
                    let py = char_y + row as i32;
                    
                    if px >= 0 && py >= 0 && px < canvas_width as i32 && py < canvas_height as i32 {
                        let idx = ((py * stride) + (px * 4)) as usize;
                        if idx + 3 < canvas.len() {
                            // Alpha 混合
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
}

/// Fallback: 简单像素字符绘制
fn draw_text_fallback(
    canvas: &mut [u8], 
    stride: i32, 
    width: u32, 
    height: u32, 
    x: i32, 
    y: i32, 
    text: &str,
    color: [u8; 4],
) {
    let mut cursor_x = x;
    for ch in text.chars() {
        draw_char_simple(canvas, stride, width, height, cursor_x, y, ch, color);
        cursor_x += 8;
    }
}

/// 简单字符绘制 (fallback)
fn draw_char_simple(canvas: &mut [u8], stride: i32, width: u32, height: u32, x: i32, y: i32, ch: char, color: [u8; 4]) {
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

impl CompositorHandler for SelectorState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // 不在 frame callback 中重绘，由 PointerHandler 直接触发
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for SelectorState {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for SelectorState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.running = false;
    }

    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
        
        if self.pool.is_none() {
            let size = (self.width * self.height * 4) as usize;
            self.pool = Some(SlotPool::new(size * 2, &self.shm_state).expect("创建 SHM pool 失败"));
        }
        
        self.draw(qh);
    }
}

impl SeatHandler for SelectorState {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(self.seat_state.get_pointer(qh, &seat).expect("获取指针失败"));
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for SelectorState {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    self.current_x = event.position.0 as i32;
                    self.current_y = event.position.1 as i32;
                    if self.is_selecting {
                        self.draw(qh);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if button == 272 { // 左键
                        self.start_x = event.position.0 as i32;
                        self.start_y = event.position.1 as i32;
                        self.current_x = self.start_x;
                        self.current_y = self.start_y;
                        self.is_selecting = true;
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == 272 && self.is_selecting { // 左键释放
                        self.is_selecting = false;
                        self.selection_complete = true;
                        self.running = false;
                    } else if button == 273 { // 右键取消
                        self.running = false;
                        self.selection_complete = false;
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for SelectorState {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm_state }
}

impl ProvidesRegistryState for SelectorState {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(SelectorState);
delegate_output!(SelectorState);
delegate_shm!(SelectorState);
delegate_seat!(SelectorState);
delegate_pointer!(SelectorState);
delegate_layer!(SelectorState);
delegate_registry!(SelectorState);

/// 运行选区选择器
pub fn select_region() -> Result<Option<CaptureRegion>> {
    let conn = Connection::connect_to_env().context("无法连接到 Wayland")?;
    let (globals, mut event_queue) = registry_queue_init(&conn).context("初始化注册表失败")?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh).context("绑定 compositor 失败")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("绑定 layer-shell 失败")?;
    let shm_state = Shm::bind(&globals, &qh).context("绑定 shm 失败")?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    let mut state = SelectorState::new(
        registry_state,
        seat_state,
        output_state,
        compositor_state,
        shm_state,
    );

    let surface = state.compositor_state.create_surface(&qh);
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("long-shot-selector"),
        None,
    );

    // 全屏覆盖，接收输入
    layer_surface.set_size(0, 0);
    layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer_surface.commit();

    state.layer_surface = Some(layer_surface);

    info!("选区选择器已启动，请拖拽选择区域...");

    while state.running {
        event_queue.blocking_dispatch(&mut state)?;
    }

    if state.selection_complete {
        let region = state.get_region();
        if region.width > 10 && region.height > 10 {
            Ok(Some(region))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}

// ============================================================================
// 边框常驻显示层 (BorderState)
// ============================================================================

/// 边框层状态
struct BorderState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,

    layer_surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    configured: bool,
    
    // 选区信息
    region: CaptureRegion,
    total_height: u32,
    
    // 运行状态
    shutdown: Arc<AtomicBool>,
    
    // wl_compositor 用于创建 region
    wl_compositor: Option<WlCompositor>,
}

impl BorderState {
    fn new(
        registry_state: RegistryState,
        output_state: OutputState,
        compositor_state: CompositorState,
        shm_state: Shm,
        region: CaptureRegion,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            registry_state,
            output_state,
            compositor_state,
            shm_state,
            layer_surface: None,
            pool: None,
            width: 1920,
            height: 1080,
            configured: false,
            region,
            total_height: region.height,
            shutdown,
            wl_compositor: None,
        }
    }

    fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::SeqCst)
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.configured || !self.is_running() {
            return;
        }

        let width = self.width;
        let height = self.height;
        let region = &self.region;
        let total_height = self.total_height;

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

        // 完全透明背景
        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            pixel[3] = 0;
        }

        // 红色边框 (2px) - 绘制在选区外面，不会被截进去
        // 边框范围：(x-2, y-2) 到 (x+w+2, y+h+2)
        let border_color: [u8; 4] = [0, 0, 255, 255]; // BGRA: 红色
        let bx = region.x - 2;  // 边框起始 X（选区外 2px）
        let by = region.y - 2;  // 边框起始 Y（选区外 2px）
        let bw = region.width as i32 + 4;   // 边框宽度
        let bh = region.height as i32 + 4;  // 边框高度
        
        for border in 0..2i32 {
            // 上边（在选区上方）
            for x in bx..(bx + bw) {
                let y = by + border;
                if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                    let idx = ((y * stride) + (x * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 下边（在选区下方）
            for x in bx..(bx + bw) {
                let y = by + bh - 1 - border;
                if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                    let idx = ((y * stride) + (x * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 左边（在选区左侧）
            for y in by..(by + bh) {
                let x = bx + border;
                if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                    let idx = ((y * stride) + (x * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
            // 右边（在选区右侧）
            for y in by..(by + bh) {
                let x = bx + bw - 1 - border;
                if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                    let idx = ((y * stride) + (x * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx..idx+4].copy_from_slice(&border_color);
                    }
                }
            }
        }
        
        // 在左上角显示尺寸信息
        Self::draw_size_label(canvas, stride, width, height, region, total_height);

        // 提交
        if let Some(ref layer_surface) = self.layer_surface {
            let surface = layer_surface.wl_surface();
            surface.attach(Some(buffer.wl_buffer()), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.frame(qh, surface.clone());
            surface.commit();
        }
    }
    
    /// 绘制尺寸标签
    fn draw_size_label(canvas: &mut [u8], stride: i32, width: u32, height: u32, region: &CaptureRegion, total_height: u32) {
        let label_x = region.x;
        let label_y = (region.y - 24).max(0);
        let label_w = 140i32;
        let label_h = 20i32;
        
        // 背景
        for dy in 0..label_h {
            for dx in 0..label_w {
                let px = label_x + dx;
                let py = label_y + dy;
                if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                    let idx = ((py * stride) + (px * 4)) as usize;
                    if idx + 3 < canvas.len() {
                        canvas[idx] = 40;      // B
                        canvas[idx + 1] = 40;  // G
                        canvas[idx + 2] = 40;  // R
                        canvas[idx + 3] = 230; // A
                    }
                }
            }
        }
        
        // 显示 宽x高(总高度)
        let text = format!("{}x{}({})", region.width, total_height, total_height);
        draw_text(canvas, stride, width, height, label_x + 4, label_y + 3, &text, 14.0, [255, 255, 255, 255]);
    }
}

impl CompositorHandler for BorderState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        if self.is_running() {
            self.draw(qh);
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for BorderState {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for BorderState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {}

    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
        
        if self.pool.is_none() {
            let size = (self.width * self.height * 4) as usize;
            self.pool = Some(SlotPool::new(size * 2, &self.shm_state).expect("创建 SHM pool 失败"));
        }
        
        self.draw(qh);
    }
}

impl ShmHandler for BorderState {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm_state }
}

impl ProvidesRegistryState for BorderState {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState];
}

// 实现 Dispatch<WlRegion, ()> 让 wl_region 事件能被处理
impl Dispatch<WlRegion, ()> for BorderState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // WlRegion 没有事件，这里不需要处理
    }
}

// 实现 Dispatch<WlCompositor, ()> 用于 globals.bind
impl Dispatch<WlCompositor, ()> for BorderState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // WlCompositor 没有事件
    }
}

delegate_compositor!(BorderState);
delegate_output!(BorderState);
delegate_shm!(BorderState);
delegate_layer!(BorderState);
delegate_registry!(BorderState);

/// 启动边框显示线程
pub fn start_border_thread(region: CaptureRegion, shutdown: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(e) = run_border_overlay(region, shutdown) {
            log::error!("边框层错误: {}", e);
        }
    })
}

/// 运行边框覆盖层
fn run_border_overlay(region: CaptureRegion, shutdown: Arc<AtomicBool>) -> Result<()> {
    let conn = Connection::connect_to_env().context("无法连接到 Wayland")?;
    let (globals, mut event_queue) = registry_queue_init(&conn).context("初始化注册表失败")?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh).context("绑定 compositor 失败")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("绑定 layer-shell 失败")?;
    let shm_state = Shm::bind(&globals, &qh).context("绑定 shm 失败")?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    // 获取 wl_compositor 用于创建 region
    let wl_compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("绑定 wl_compositor 失败")?;

    let mut state = BorderState::new(
        registry_state,
        output_state,
        compositor_state,
        shm_state,
        region,
        shutdown.clone(),
    );
    state.wl_compositor = Some(wl_compositor.clone());

    let surface = state.compositor_state.create_surface(&qh);
    
    // 创建空的 input_region，让鼠标事件穿透
    let empty_region = wl_compositor.create_region(&qh, ());
    surface.set_input_region(Some(&empty_region));
    
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("long-shot-border"),
        None,
    );

    // 全屏透明覆盖
    layer_surface.set_size(0, 0);
    layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.commit();

    state.layer_surface = Some(layer_surface);

    info!("边框显示层已启动（鼠标穿透）");

    while state.is_running() {
        event_queue.blocking_dispatch(&mut state)?;
    }

    Ok(())
}
