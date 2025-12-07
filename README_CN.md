# Long Shot RS

[English](README.md)

高性能原生 **长截图**（滚动截图）应用，专为 **Linux Wayland** 设计。

![Platform](https://img.shields.io/badge/平台-Linux%20Wayland-blue)
![License](https://img.shields.io/badge/许可证-MIT-green)

## 功能特性

- **原生 Wayland 集成**：直接使用 `wlr-screencopy-unstable-v1` 协议截屏
- **实时拼接**：基于 OpenCV 的图像拼接，采用 Sobel 梯度 + 模板匹配算法
- **低延迟**：通过 evdev 直接监控滚轮事件
- **置顶预览窗口**：使用 Layer Shell 实现始终置顶的预览窗口
- **高质量导出**：全分辨率 PNG 保存或复制到剪贴板

## 实现原理

### 整体架构

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  线程 A          │     │  线程 B           │    │  线程 C         │
│  输入监控        │────▶ │  工作线程         │────▶│  覆盖层窗口     │
│  (evdev)        │     │  (截图+拼接)      │     │  (Layer Shell)  │
└─────────────────┘     └──────────────────┘     └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
   /dev/input/*           wlr-screencopy            预览窗口
   滚轮事件               帧捕获                   保存/复制 UI
```

### 模块说明

| 模块 | 文件 | 功能描述 |
|------|------|----------|
| **main** | `main.rs` | 程序入口，线程编排 |
| **input** | `input.rs` | evdev 滚轮事件监控 |
| **capture** | `capture.rs` | Wayland 屏幕截图 (`wlr-screencopy`) |
| **stitch** | `stitch.rs` | OpenCV 图像拼接算法 |
| **worker** | `worker.rs` | 截图 + 拼接流水线 |
| **overlay** | `overlay.rs` | Layer Shell 置顶预览窗口 |
| **types** | `types.rs` | 共享数据结构和通道定义 |

### 拼接算法详解

#### 1. 帧捕获
通过 `zwlr_screencopy_manager_v1` 协议捕获屏幕指定区域。使用 SHM 共享内存缓冲区，避免不必要的内存拷贝。

#### 2. 图像预处理
```
原始帧 → 灰度化 → Sobel 梯度（边缘检测）
```
Sobel 梯度处理能够：
- 提取图像边缘特征，增强匹配精度
- 对低对比度内容（如浅色背景上的浅色文字）更敏感
- 减少光照变化的影响

#### 3. 模板匹配
```
当前结果图 → 提取底部模板 → 在新帧中搜索匹配位置
```
- 从当前拼接结果的底部提取一块区域作为模板
- 在新帧中使用 `cv::matchTemplate` 搜索最佳匹配位置
- 应用惯性约束（根据滚动方向限制搜索范围）

#### 4. 重叠检测与拼接
```
找到匹配位置 → 计算重叠区域 → 追加新内容
```

**核心参数**：
| 参数 | 默认值 | 说明 |
|------|--------|------|
| `IGNORE_Y_TOP` | 15% | 忽略帧顶部（避开固定导航栏） |
| `IGNORE_Y_BOTTOM` | 15% | 忽略帧底部（避开固定状态栏） |
| `TEMPLATE_RATIO` | 20% | 模板占帧高度的比例 |
| `MATCH_CONFIDENCE` | 0.5 | 最小匹配置信度 |
| `MAX_SEARCH_RANGE` | 500px | 惯性约束搜索范围 |

### Layer Shell 覆盖层

使用 `wlr-layer-shell-unstable-v1` 协议创建置顶预览窗口：

- **层级**：`Overlay`（最高 z-order，始终在最前）
- **定位**：锚定到屏幕边缘，位于选区右侧
- **渲染**：透明背景，自定义按钮绘制（圆角矩形 + 图标）
- **缩放**：Box Filter 下采样，保持预览清晰度

### 输入监控

通过 evdev 直接读取输入设备：
1. 扫描 `/dev/input/event*` 设备
2. 筛选具有 `REL_WHEEL` 能力的设备（鼠标滚轮）
3. 监控滚轮事件，带 50ms 防抖
4. 发送截图触发信号给工作线程

## 依赖说明

### 编译依赖

编译时需要的开发库和工具：

```bash
# Arch Linux
sudo pacman -S rust opencv clang cmake pkg-config

# Debian/Ubuntu
sudo apt install cargo rustc libopencv-dev libclang-dev cmake pkg-config

# Fedora
sudo dnf install rust cargo opencv-devel clang-devel cmake pkg-config
```

**说明**：
- `rust`/`cargo`：Rust 编译器和包管理器
- `opencv`：图像处理库（需要开发头文件）
- `clang`：opencv-rust 绑定需要 libclang
- `cmake`：构建系统
- `pkg-config`：库路径发现工具

### 运行依赖

运行时需要的库和工具：

```bash
# Arch Linux
sudo pacman -S opencv wl-clipboard

# Debian/Ubuntu
sudo apt install libopencv-core4.* libopencv-imgproc4.* wl-clipboard

# Fedora
sudo dnf install opencv wl-clipboard
```

**说明**：
- `opencv`：运行时链接的共享库
- `wl-clipboard`：Wayland 剪贴板工具（arboard 后端）

### Wayland 混成器要求

需要基于 **wlroots** 的混成器，支持以下协议：
- `wlr-screencopy-unstable-v1`：屏幕截图
- `wlr-layer-shell-unstable-v1`：覆盖层窗口

**支持的混成器**：
- ✅ Sway
- ✅ Hyprland
- ✅ river
- ✅ wayfire
- ✅ labwc

**不支持**：
- ❌ GNOME（使用不同的 portal 协议）
- ❌ KDE Plasma（使用不同的协议）

### 输入权限

需要读取 `/dev/input/event*` 设备的权限：

```bash
# 将用户添加到 input 组（推荐）
sudo usermod -aG input $USER

# 然后注销并重新登录
```

## 编译

```bash
git clone https://github.com/yourname/long-shot-rs
cd long-shot-rs

# Release 构建（推荐，启用优化）
cargo build --release

# 可执行文件位置：./target/release/long-shot-rs
```

## 使用方法

```bash
./target/release/long-shot-rs [选项]
```

### 命令行选项

| 选项 | 说明 |
|------|------|
| `-o, --output <路径>` | 输出文件路径（不指定则自动生成带时间戳的文件名） |
| `-d, --save-dir <目录>` | 自动命名文件的保存目录（默认：~/Pictures） |
| `-e, --exec <命令>` | 保存后执行的命令，使用 `{}` 作为文件路径占位符 |
| `-h, --help` | 显示帮助信息 |
| `-V, --version` | 显示版本号 |

### 使用示例

```bash
# 基本用法 - 保存到 ~/Pictures/longshot_年月日_时分秒.png
./target/release/long-shot-rs

# 保存到指定文件
./target/release/long-shot-rs -o ~/screenshot.png

# 保存到自定义目录
./target/release/long-shot-rs -d ~/Screenshots/

# 保存后自动打开图片
./target/release/long-shot-rs -e "xdg-open {}"

# 使用指定图片查看器打开
./target/release/long-shot-rs -e "imv {}"

# 保存后将路径复制到剪贴板
./target/release/long-shot-rs -e "echo {} | wl-copy"
```

### 操作步骤

1. **选择区域**：拖拽选择要截图的滚动区域
2. **滚动内容**：在目标窗口中缓慢滚动
3. **查看预览**：实时观察拼接效果
4. **导出图片**：点击保存 (💾)、复制 (📋) 或取消 (✕) 按钮

### 使用技巧

- 只选择可滚动内容区域，排除固定的标题栏/工具栏
- 缓慢滚动以获得更好的拼接精度
- 对文字密集的内容效果最佳（文档、网页等）
- 避免选择大面积空白区域

## 故障排除

| 问题 | 解决方案 |
|------|----------|
| "No scroll devices found" | 将用户添加到 `input` 组 |
| "Compositor does not support..." | 使用 wlroots 混成器 |
| 拼接效果差 | 滚动更慢，避免空白区域 |
| 预览窗口不显示 | 检查混成器是否支持 layer-shell |
| 选区无法选择 | 检查混成器是否支持 layer-shell |

## Rust 依赖库

```toml
wayland-client = "0.31"          # Wayland 协议客户端
wayland-protocols-wlr = "0.3"    # wlroots 扩展协议
smithay-client-toolkit = "0.19"  # Layer Shell 工具包
opencv = "0.93"                  # 图像处理
evdev = "0.12"                   # 输入设备监控
arboard = "3.4"                  # 剪贴板操作
crossbeam-channel = "0.5"        # 线程间通信
nix = "0.29"                     # Unix 系统调用
```

## 许可证

MIT

## 致谢

- [smithay-client-toolkit](https://github.com/Smithay/client-toolkit) - Wayland 客户端工具包
- [wayland-rs](https://github.com/Smithay/wayland-rs) - Wayland 协议 Rust 绑定
- [OpenCV](https://opencv.org/) - 图像处理库
