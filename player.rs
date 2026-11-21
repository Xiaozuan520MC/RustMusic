use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};

const FFMPEG_CHANNELS: u16 = 2;
const FFMPEG_SAMPLE_RATE: u32 = 44_100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Native,
    Ffmpeg,
}

pub struct Player {
    stream: Option<OutputStream>,
    init_error: Option<String>,
    sink: Option<Sink>,
    state: PlaybackState,
    volume: f32,
    speed: f32,
    path: Option<PathBuf>,
    backend: Backend,
    base_position: Duration,
}

impl Player {
    pub fn new() -> Self {
        let (stream, init_error) = match OutputStreamBuilder::open_default_stream() {
            Ok(stream) => (Some(stream), None),
            Err(err) => (None, Some(format!("无法打开音频输出设备：{err}"))),
        };

        Self {
            stream,
            init_error,
            sink: None,
            state: PlaybackState::Stopped,
            volume: 0.8,
            speed: 1.0,
            path: None,
            backend: Backend::Native,
            base_position: Duration::ZERO,
        }
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(sink) = &self.sink {
            sink.set_volume(self.volume);
        }
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.25, 4.0);
        if let Some(sink) = &self.sink {
            sink.set_speed(self.speed);
        }
    }

    /// 优先使用内置的纯 Rust 解码器；遇到不支持的编码（如 8-bit 特殊封装、
    /// Opus、WMA 等）时自动回退到 ffmpeg 管道解码。
    pub fn play_file(&mut self, path: &Path) -> Result<(), String> {
        if self.stream.is_none() {
            return Err(self
                .init_error
                .clone()
                .unwrap_or_else(|| "音频输出不可用".to_string()));
        }

        let file = File::open(path).map_err(|err| format!("无法打开文件：{err}"))?;
        match Decoder::new(BufReader::new(file)) {
            Ok(decoder) => {
                let sink = self.make_sink()?;
                sink.append(decoder);
                self.activate(sink, path, Backend::Native, Duration::ZERO);
                Ok(())
            }
            Err(native_err) => {
                let source = spawn_ffmpeg(path, Duration::ZERO).map_err(|ffmpeg_err| {
                    format!("内置解码器失败（{native_err}），ffmpeg 回退失败（{ffmpeg_err}）")
                })?;
                let sink = self.make_sink()?;
                sink.append(source);
                self.activate(sink, path, Backend::Ffmpeg, Duration::ZERO);
                Ok(())
            }
        }
    }

    fn make_sink(&self) -> Result<Sink, String> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| self.init_error.clone().unwrap_or_else(|| "音频输出不可用".to_string()))?;
        let sink = Sink::connect_new(stream.mixer());
        sink.set_volume(self.volume);
        sink.set_speed(self.speed);
        Ok(sink)
    }

    fn activate(&mut self, sink: Sink, path: &Path, backend: Backend, offset: Duration) {
        if let Some(old_sink) = self.sink.take() {
            old_sink.stop();
        }
        sink.play();
        self.sink = Some(sink);
        self.state = PlaybackState::Playing;
        self.path = Some(path.to_path_buf());
        self.backend = backend;
        self.base_position = offset;
    }

    pub fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
            self.state = PlaybackState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if let Some(sink) = &self.sink {
            sink.play();
            self.state = PlaybackState::Playing;
        }
    }

    pub fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.state = PlaybackState::Stopped;
        self.path = None;
        self.backend = Backend::Native;
        self.base_position = Duration::ZERO;
    }

    pub fn seek(&mut self, position: Duration) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("当前没有正在播放的曲目".to_string());
        };

        match self.backend {
            Backend::Native => {
                let sink = self
                    .sink
                    .as_ref()
                    .ok_or_else(|| "当前没有正在播放的曲目".to_string())?;
                sink.try_seek(position).map_err(|err| err.to_string())
            }
            Backend::Ffmpeg => {
                let source = spawn_ffmpeg(&path, position)?;
                let was_paused = self.state == PlaybackState::Paused;
                let sink = self.make_sink()?;
                if was_paused {
                    sink.pause();
                }
                sink.append(source);

                if let Some(old_sink) = self.sink.take() {
                    old_sink.stop();
                }
                self.sink = Some(sink);
                self.base_position = position;
                Ok(())
            }
        }
    }

    pub fn position(&self) -> Duration {
        self.base_position
            + self
                .sink
                .as_ref()
                .map(Sink::get_pos)
                .unwrap_or(Duration::ZERO)
    }

    /// 当前曲目是否自然播放结束。
    pub fn finished(&self) -> bool {
        self.state == PlaybackState::Playing
            && self
                .sink
                .as_ref()
                .map(|sink| sink.empty())
                .unwrap_or(false)
    }
}

/// 由 ffmpeg 输出的 16-bit 立体声 PCM 流构造 rodio 音源。
struct FfmpegSource {
    child: Child,
    stdout: ChildStdout,
    buffer: Vec<u8>,
    offset: usize,
    done: bool,
}

impl Iterator for FfmpegSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if self.offset + 2 <= self.buffer.len() {
                let sample =
                    i16::from_le_bytes([self.buffer[self.offset], self.buffer[self.offset + 1]]);
                self.offset += 2;
                return Some(sample as f32 / 32_768.0);
            }
            if self.done {
                return None;
            }

            self.buffer.clear();
            self.offset = 0;
            let mut chunk = [0u8; 16_384];
            match self.stdout.read(&mut chunk) {
                Ok(0) => {
                    self.done = true;
                    let _ = self.child.wait();
                    return None;
                }
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.done = true;
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return None;
                }
            }
        }
    }
}

impl Source for FfmpegSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        FFMPEG_CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        FFMPEG_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_ffmpeg(path: &Path, start: Duration) -> Result<FfmpegSource, String> {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin");

    if !start.is_zero() {
        command.arg("-ss").arg(format!("{:.3}", start.as_secs_f64()));
    }

    command
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:a:0")
        .arg("-vn")
        .arg("-ac")
        .arg(FFMPEG_CHANNELS.to_string())
        .arg("-ar")
        .arg(FFMPEG_SAMPLE_RATE.to_string())
        .arg("-acodec")
        .arg("pcm_s16le")
        .arg("-f")
        .arg("s16le")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|err| format!("无法启动 ffmpeg：{err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 ffmpeg 输出".to_string())?;

    Ok(FfmpegSource {
        child,
        stdout,
        buffer: Vec::with_capacity(16_384),
        offset: 0,
        done: false,
    })
}
