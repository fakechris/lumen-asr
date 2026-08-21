//! ffmpeg helpers shared by the headless CLI and GUI meeting import.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const MEETING_IMPORT_EXTENSIONS: &[&str] = &["wav", "wave", "mp3", "m4a", "mp4"];

pub fn audio_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn is_importable_meeting_audio(path: &Path) -> bool {
    MEETING_IMPORT_EXTENSIONS.contains(&audio_extension(path).as_str())
}

/// Convert any ffmpeg-readable input to 16 kHz mono PCM WAV at `dest`.
pub fn convert_to_wav_16k(src: &Path, dest: &Path) -> Result<(), String> {
    if !src.is_file() {
        return Err(format!("找不到音频文件：{}", src.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("无法创建输出目录：{e}"))?;
    }
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            &src.display().to_string(),
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("无法启动 ffmpeg（{e}）。请安装 ffmpeg 后再导入 m4a/mp3/mp4。"))?;
    if !status.success() {
        return Err(format!("ffmpeg 转换失败：{}", src.display()));
    }
    if !dest.is_file() {
        return Err("ffmpeg 没有生成 wav 文件".into());
    }
    Ok(())
}

/// Copy a wav as-is, otherwise convert through ffmpeg into `dest`.
pub fn copy_or_convert_to_wav(src: &Path, dest: &Path) -> Result<(), String> {
    let ext = audio_extension(src);
    if matches!(ext.as_str(), "wav" | "wave") {
        if !src.is_file() {
            return Err(format!("找不到音频文件：{}", src.display()));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("无法创建会议目录：{e}"))?;
        }
        std::fs::copy(src, dest).map_err(|e| format!("无法复制音频：{e}"))?;
        Ok(())
    } else {
        convert_to_wav_16k(src, dest)
    }
}

/// Convert compressed audio to a temp 16 kHz wav. WAV inputs are returned as-is.
pub fn ensure_wav(path: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
    let ext = audio_extension(path);
    if matches!(ext.as_str(), "wav" | "wave") {
        if !path.is_file() {
            return Err(format!("找不到音频文件：{}", path.display()));
        }
        return Ok((path.to_path_buf(), None));
    }
    let tmp = std::env::temp_dir().join(format!(
        "lumen-audio-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("create temp dir: {e}"))?;
    let out = tmp.join("input.16k.wav");
    if let Err(error) = convert_to_wav_16k(path, &out) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(error);
    }
    Ok((out, Some(tmp)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importable_extensions() {
        assert!(is_importable_meeting_audio(Path::new("talk.m4a")));
        assert!(is_importable_meeting_audio(Path::new("talk.MP3")));
        assert!(is_importable_meeting_audio(Path::new("talk.wav")));
        assert!(is_importable_meeting_audio(Path::new("talk.mp4")));
        assert!(!is_importable_meeting_audio(Path::new("talk.txt")));
        assert!(!is_importable_meeting_audio(Path::new("talk")));
    }

    #[test]
    fn copies_wav_without_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.wav");
        let dest = dir.path().join("nested").join("out.wav");
        std::fs::write(&src, b"RIFF....WAVEfmt ").unwrap();
        copy_or_convert_to_wav(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"RIFF....WAVEfmt ");
    }
}
