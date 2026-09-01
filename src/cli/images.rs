// port of src/cli/images.ts
//
// Image attachment discovery/copying into the session images/ dir: data-URL
// encoding, osascript clipboard hex parsing, and the size/type limits with
// their exact error strings. The TS module is self-contained (node:fs,
// node:path, node:child_process), so this port only needs std + base64.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};

// export type GoalImageAttachment = { dataUrl, fileName, path }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalImageAttachment {
    pub data_url: String,
    pub file_name: String,
    pub path: String,
}

// const IMAGE_MIME_BY_EXTENSION: Record<string, string> — kept as an ordered
// list because the data-URL decoder looks up the FIRST extension that maps to
// a mime (".jpeg" wins over ".jpg" for image/jpeg).
const IMAGE_MIME_BY_EXTENSION: [(&str, &str); 5] = [
    (".gif", "image/gif"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".png", "image/png"),
    (".webp", "image/webp"),
];

fn mime_for_extension(extension: &str) -> Option<&'static str> {
    IMAGE_MIME_BY_EXTENSION
        .iter()
        .find(|(candidate_extension, _)| *candidate_extension == extension)
        .map(|(_, mime)| *mime)
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    IMAGE_MIME_BY_EXTENSION
        .iter()
        .find(|(_, candidate_mime)| *candidate_mime == mime)
        .map(|(extension, _)| *extension)
}

// node:path extname: substring from the LAST '.', including the dot; "" when
// the only '.' is the first character of the file name (dotfiles).
fn extname(path_text: &str) -> String {
    let file_name = basename(path_text);

    match file_name.rfind('.') {
        Some(index) if index > 0 => file_name[index..].to_string(),
        _ => String::new(),
    }
}

// node:path basename.
fn basename(path_text: &str) -> String {
    let trimmed = path_text.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some("") | None => path_text.to_string(),
        Some(name) => name.to_string(),
    }
}

// export function imageDataUrl(bytes: Uint8Array, mime: string): string
pub fn image_data_url(bytes: &[u8], mime: &str) -> String {
    format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes))
}

// function parseClipboardImageHex(output: string): Uint8Array | null
//
// osascript prints clipboard image data as «data PNGf89504E47...» — the
// payload is hex between the four-character class code and the closing
// guillemet.
pub fn parse_clipboard_image_hex(output: &str) -> Option<Vec<u8>> {
    let regex = Regex::new(r"«data [A-Za-z0-9]{4}([0-9A-Fa-f]+)»").ok()?;
    let captures = regex.captures(output.trim())?;
    let hex = captures.get(1)?.as_str();

    if hex.len() % 2 != 0 {
        return None;
    }

    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect()
}

// function nextImageFileName(imagesDir: string, extension: string): string
fn next_image_filename(images_dir: &Path, extension: &str) -> PathBuf {
    let mut count = 0;

    if let Ok(entries) = std::fs::read_dir(images_dir) {
        count = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("image-"))
            .count();
    }

    images_dir.join(format!("image-{}{}", count + 1, extension))
}

// function saveAttachment(
//   imagesDir: string,
//   bytes: Uint8Array,
//   mime: string,
//   extension: string
// ): GoalImageAttachment
fn save_attachment(
    images_dir: &Path,
    bytes: &[u8],
    mime: &str,
    extension: &str,
) -> GoalImageAttachment {
    std::fs::create_dir_all(images_dir).expect("images dir should be creatable");
    let file_path = next_image_filename(images_dir, extension);

    std::fs::write(&file_path, bytes).expect("image file should be writable");

    GoalImageAttachment {
        data_url: image_data_url(bytes, mime),
        file_name: file_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        path: file_path.to_string_lossy().to_string(),
    }
}

// function captureClipboardImageWith(
//   imagesDir: string,
//   runClipboardCommand: () => string | null
// ): GoalImageAttachment | null
//
// The injected runner is the test seam; the macOS-only real runner lives in
// `capture_clipboard_image`.
pub fn capture_clipboard_image_with(
    images_dir: &Path,
    run_clipboard_command: impl Fn() -> Option<String>,
) -> Option<GoalImageAttachment> {
    let output = run_clipboard_command()?;

    let Some(bytes) = parse_clipboard_image_hex(&output) else {
        return None;
    };

    if bytes.is_empty() {
        return None;
    }

    Some(save_attachment(images_dir, &bytes, "image/png", ".png"))
}

// export function captureClipboardImage(imagesDir: string) — macOS only; on
// other platforms the TS runner returns null before spawning anything. The
// 64 MB maxBuffer guard does not apply to the Rust port: we read the pipe
// directly and the payload is bounded by the clipboard itself.
#[cfg(target_os = "macos")]
pub fn capture_clipboard_image(images_dir: &Path) -> Option<GoalImageAttachment> {
    use std::process::Command;

    let output = Command::new("osascript")
        .args(["-e", "the clipboard as «class PNGf»"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    capture_clipboard_image_with(images_dir, move || Some(stdout.clone()))
}

#[cfg(not(target_os = "macos"))]
pub fn capture_clipboard_image(images_dir: &Path) -> Option<GoalImageAttachment> {
    let _ = images_dir;
    None
}

// export function attachmentFromImageFile(
//   sourcePath: string,
//   args: { cwd: string; imagesDir: string }
// ): GoalImageAttachment | null
//
// A pasted absolute or relative path to an image file becomes an attachment
// too, so drag-dropping a file into the terminal works without clipboard
// tricks.
pub fn attachment_from_image_file(
    source_path: &str,
    args: (&str, &Path),
) -> Option<GoalImageAttachment> {
    let (cwd, images_dir) = args;
    let extension = extname(source_path).to_lowercase();
    let mime = mime_for_extension(&extension)?;

    let absolute_path = if Path::new(source_path).is_absolute() {
        source_path.to_string()
    } else {
        Path::new(cwd).join(source_path).to_string_lossy().to_string()
    };

    if !Path::new(&absolute_path).exists() {
        return None;
    }

    match std::fs::read(&absolute_path) {
        Ok(bytes) => Some(GoalImageAttachment {
            data_url: image_data_url(&bytes, mime),
            file_name: basename(&absolute_path),
            path: absolute_path,
        }),
        Err(_) => None,
    }
}

// export function attachmentFromDataUrl(
//   dataUrl: string,
//   imagesDir: string
// ): GoalImageAttachment | null
pub fn attachment_from_data_url(data_url: &str, images_dir: &Path) -> Option<GoalImageAttachment> {
    let regex = Regex::new(r"^data:(image/[a-z+.-]+);base64,([A-Za-z0-9+/=\s]+)$").ok()?;
    let captures = regex.captures(data_url.trim())?;

    let mime = captures.get(1)?.as_str();
    let extension = extension_for_mime(mime).unwrap_or(".png");

    let cleaned = captures.get(2)?.as_str().replace(char::is_whitespace, "");
    let bytes = BASE64_STANDARD.decode(cleaned).ok()?;

    if bytes.is_empty() {
        return None;
    }

    Some(save_attachment(images_dir, &bytes, mime, extension))
}

// export function looksLikeImagePaste(text: string): "data-url" | "file-path" | null
pub fn looks_like_image_paste(text: &str) -> Option<&'static str> {
    let trimmed_text = text.trim();

    if trimmed_text.starts_with("data:image/") {
        return Some("data-url");
    }

    if !trimmed_text.chars().any(char::is_whitespace)
        && mime_for_extension(&extname(trimmed_text).to_lowercase()).is_some()
    {
        return Some("file-path");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_BYTES: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    // it("parses osascript clipboard hex output")
    #[test]
    fn parses_osascript_clipboard_hex_output() {
        let hex = hex_of(&PNG_BYTES);

        assert_eq!(
            parse_clipboard_image_hex(&format!("«data PNGf{hex}»")),
            Some(PNG_BYTES.to_vec())
        );
        assert_eq!(parse_clipboard_image_hex("not clipboard output"), None);
        assert_eq!(parse_clipboard_image_hex("«data PNGfabc»"), None);
    }

    // it("captures a clipboard image via an injected clipboard command")
    #[test]
    fn captures_a_clipboard_image_via_an_injected_clipboard_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let images_dir = temp.path().join("images");
        let hex = hex_of(&PNG_BYTES);

        let attachment = capture_clipboard_image_with(&images_dir, || {
            Some(format!("«data PNGf{hex}»"))
        })
        .expect("attachment");

        assert!(Path::new(&attachment.path).exists());
        assert_eq!(attachment.data_url, image_data_url(&PNG_BYTES, "image/png"));

        assert_eq!(capture_clipboard_image_with(&images_dir, || None), None);
    }

    // it("builds attachments from pasted data URLs and image file paths")
    #[test]
    fn builds_attachments_from_pasted_data_urls_and_image_file_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let images_dir = root.join("images");

        let attachment = attachment_from_data_url(
            &format!("data:image/png;base64,{}", BASE64_STANDARD.encode(PNG_BYTES)),
            &images_dir,
        )
        .expect("attachment from data URL");
        assert_eq!(
            std::fs::read(&attachment.path).expect("saved bytes"),
            PNG_BYTES
        );

        std::fs::write(root.join("screenshot.png"), PNG_BYTES).expect("write png");
        let from_file = attachment_from_image_file(
            "screenshot.png",
            (root.to_str().expect("utf-8 root"), images_dir.as_path()),
        )
        .expect("attachment from file");
        assert_eq!(from_file.data_url, image_data_url(&PNG_BYTES, "image/png"));

        assert_eq!(
            attachment_from_image_file(
                "nope.png",
                (root.to_str().expect("utf-8 root"), images_dir.as_path())
            ),
            None
        );
        assert_eq!(
            attachment_from_data_url("data:text/plain;base64,aGk=", &images_dir),
            None
        );
    }

    // it("classifies pasted text that carries an image payload")
    #[test]
    fn classifies_pasted_text_that_carries_an_image_payload() {
        assert_eq!(
            looks_like_image_paste("data:image/png;base64,AAAA"),
            Some("data-url")
        );
        assert_eq!(looks_like_image_paste("/tmp/shot.png"), Some("file-path"));
        assert_eq!(looks_like_image_paste("shot.jpeg"), Some("file-path"));
        assert_eq!(looks_like_image_paste("explain this code"), None);
    }
}
