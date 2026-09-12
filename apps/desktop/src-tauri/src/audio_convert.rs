//! ffmpeg helpers shared by the headless CLI and GUI meeting import.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const MEETING_IMPORT_EXTENSIONS: &[&str] = &["wav", "wave", "mp3", "m4a", "mp4", "opus", "ogg"];

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

/// Convert any ffmpeg-readable input to an MP3 file at `dest`.
pub fn convert_to_mp3(src: &Path, dest: &Path) -> Result<(), String> {
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
            "-vn",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ])
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("无法启动 ffmpeg（{e}）。导出 MP3 需要安装 ffmpeg。"))?;
    if !status.success() {
        return Err(format!("ffmpeg 导出 MP3 失败：{}", src.display()));
    }
    if !dest.is_file() {
        return Err("ffmpeg 没有生成 mp3 文件".into());
    }
    Ok(())
}

/// Convert any ffmpeg-readable input to an Ogg Vorbis file at `dest`.
pub fn convert_to_ogg(src: &Path, dest: &Path) -> Result<(), String> {
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
            "-vn",
            "-c:a",
            "libvorbis",
            "-q:a",
            "4",
        ])
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("无法启动 ffmpeg（{e}）。导出 OGG 需要安装 ffmpeg。"))?;
    if !status.success() {
        return Err(format!("ffmpeg 导出 OGG 失败：{}", src.display()));
    }
    if !dest.is_file() {
        return Err("ffmpeg 没有生成 ogg 文件".into());
    }
    Ok(())
}

/// Copy a wav as-is, otherwise convert through ffmpeg into `dest`.
pub fn copy_or_convert_to_wav(src: &Path, dest: &Path) -> Result<(), String> {
    let ext = audio_extension(src);
    if matches!(ext.as_str(), "wav" | "wave") {
        copy_audio_file(src, dest, "无法复制音频")
    } else {
        convert_to_wav_16k(src, dest)
    }
}

/// Plain file copy into the meeting library (used for WAV and Opus imports,
/// both of which the pipeline reads without an ffmpeg conversion).
pub fn copy_audio_file(src: &Path, dest: &Path, err_prefix: &str) -> Result<(), String> {
    if !src.is_file() {
        return Err(format!("找不到音频文件：{}", src.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("无法创建会议目录：{e}"))?;
    }
    std::fs::copy(src, dest).map_err(|e| format!("{err_prefix}：{e}"))?;
    Ok(())
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
        assert!(is_importable_meeting_audio(Path::new("talk.opus")));
        assert!(is_importable_meeting_audio(Path::new("talk.OGG")));
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

    #[test]
    fn convert_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp3");
        let err = convert_to_mp3(Path::new("/path/that/does/not/exist.wav"), &dest).unwrap_err();
        assert!(err.contains("找不到音频文件"));

        let ogg_dest = dir.path().join("out.ogg");
        let err =
            convert_to_ogg(Path::new("/path/that/does/not/exist.wav"), &ogg_dest).unwrap_err();
        assert!(err.contains("找不到音频文件"));
    }

    #[test]
    fn convert_wav_to_mp3_and_ogg_with_ffmpeg() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("test.wav");
        let samples = vec![0.0f32; 1600];
        let wav_bytes = lumen_asr::pcm_to_wav_bytes(&samples, 16000);
        std::fs::write(&wav_path, &wav_bytes).unwrap();

        let mp3_path = dir.path().join("test.mp3");
        convert_to_mp3(&wav_path, &mp3_path).expect("convert to mp3");
        assert!(mp3_path.is_file());
        assert!(std::fs::metadata(&mp3_path).unwrap().len() > 0);

        let ogg_path = dir.path().join("test.ogg");
        convert_to_ogg(&wav_path, &ogg_path).expect("convert to ogg");
        assert!(ogg_path.is_file());
        assert!(std::fs::metadata(&ogg_path).unwrap().len() > 0);
    }
}
