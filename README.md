# RustMusic

纯 Rust + GTK4 写的本地音乐播放器，轻量、无广告、不需要联网。

当前版本：**v0.1.1**

## 功能

- 导入本地文件夹并自动扫描：MP3、FLAC、WAV、OGG、M4A、AAC 等
- 通过 lofty 读取标题、艺术家、专辑等标签，ffprobe 兜底时长
- 搜索、收藏夹、仅显示收藏
- LRC 歌词逐句滚动，中英双语自动合并，当前句始终居中
- 0.5–2.0 倍速、音量记忆、随机与循环播放
- 中文 / English 界面随时切换
- 最小化后后台播放，支持系统托盘
- 内置解码器不支持时自动回退到 ffmpeg

## 依赖

构建：

- Rust（2021 edition）
- GTK 4.10 或更高版本
- pkg-config、C 编译器

运行：

- ffmpeg / ffprobe（可选，用于解码与时长兜底）

Ubuntu / Debian：

```bash
sudo apt install libgtk-4-dev build-essential pkg-config ffmpeg
```

## 构建与运行

```bash
cargo build --release
```

## 配置

配置保存在 `~/.config/rustmusic/config`，包含音乐文件夹、音量、收藏、删除记录和界面语言。

## License

[MIT](LICENSE) © 2026 Kernel Panic Cat
