use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};

pub const UNKNOWN_ARTIST: &str = "未知艺术家";
pub const UNKNOWN_ALBUM: &str = "未知专辑";

const UNKNOWN_TITLE: &str = "未知曲目";
const MAX_SCAN_DEPTH: u32 = 16;

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp2", "mp1", "flac", "wav", "wave", "ogg", "oga", "m4a", "m4b", "mp4", "aac",
    "aif", "aiff", "aifc", "caf", "mka", "opus", "wma", "ape", "wv", "mpc",
];

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Duration,
    track_no: Option<u32>,
    search_key: String,
}

impl Track {
    /// 供搜索框做大小写无关的模糊匹配。
    pub fn matches(&self, query_lower: &str) -> bool {
        query_lower.is_empty() || self.search_key.contains(query_lower)
    }

    pub fn subtitle(&self) -> String {
        if self.album == UNKNOWN_ALBUM {
            self.artist.clone()
        } else {
            format!("{} · {}", self.artist, self.album)
        }
    }
}

/// 递归扫描目录，读取音频标签；无法解析标签时退回文件名。
pub fn scan_folder(root: &Path) -> Vec<Track> {
    let mut tracks = Vec::new();
    walk(root, &mut tracks, 0);

    tracks.sort_by(|a, b| {
        a.artist
            .to_lowercase()
            .cmp(&b.artist.to_lowercase())
            .then_with(|| a.album.to_lowercase().cmp(&b.album.to_lowercase()))
            .then_with(|| {
                a.track_no
                    .unwrap_or(u32::MAX)
                    .cmp(&b.track_no.unwrap_or(u32::MAX))
            })
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });

    tracks
}

fn walk(dir: &Path, out: &mut Vec<Track>, depth: u32) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out, depth + 1);
        } else if is_audio_file(&path) {
            out.push(read_track(&path));
        }
    }
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn read_track(path: &Path) -> Track {
    let mut title = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| UNKNOWN_TITLE.to_string());
    let mut artist = UNKNOWN_ARTIST.to_string();
    let mut album = UNKNOWN_ALBUM.to_string();
    let mut duration = Duration::ZERO;
    let mut track_no = None;

    if let Ok(tagged) = Probe::open(path).and_then(|probe| probe.read()) {
        duration = tagged.properties().duration();
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(value) = tag.title().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                title = value;
            }
            if let Some(value) = tag.artist().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                artist = value;
            }
            if let Some(value) = tag.album().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                album = value;
            }
            track_no = tag.track();
        }
    }

    // 少数容器（如 WMA）无法由 lofty 读取时长，交给 ffprobe 兜底。
    if duration.is_zero() {
        duration = ffprobe_duration(path).unwrap_or(Duration::ZERO);
    }

    let search_key = format!("{} {} {}", title, artist, album).to_lowercase();

    Track {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        duration,
        track_no,
        search_key,
    }
}

/// 读取内嵌歌词（MP3 的 USLT、FLAC 的 LYRICS / UNSYNCEDLYRICS）。
pub fn embedded_lyrics(path: &Path) -> Option<String> {
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let lyrics = tag
        .get_strings(&ItemKey::Lyrics)
        .find(|text| !text.trim().is_empty())
        .map(|text| text.to_string());
    lyrics
}

fn ffprobe_duration(path: &Path) -> Option<Duration> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let seconds = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()?;
    if seconds.is_finite() && seconds > 0.0 {
        Some(Duration::from_secs_f64(seconds))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_silent_wav(path: &Path, seconds: u32) {
        let rate = 44_100u32;
        let samples = rate * seconds;
        let data_len = samples * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + data_len as usize, 0);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scans_supported_files_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("rustmusic_scan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        write_silent_wav(&dir.join("nested/Hello.wav"), 1);
        fs::write(dir.join("notes.txt"), "not music").unwrap();

        let tracks = scan_folder(&dir);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Hello");
        assert!(tracks[0].duration.as_secs() >= 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
