//! Inline images for the TUI.
//!
//! Two escape protocols cover the terminals operators actually run:
//!
//! * **kitty graphics protocol** (kitty, ghostty) — `ESC _ G ... ESC \`,
//!   transmit-and-display (`a=T`, `f=100` for PNG), with the base64 payload
//!   split into <=4096-byte chunks and `m=1` on every chunk but the last.
//! * **iTerm2 inline images** (iTerm2, WezTerm) — `ESC ] 1337 ; File=...`,
//!   which carries any image format plus a cell-width hint.
//!
//! Detection is environment based and never leaks into a non-interactive
//! run: the TUI seeds the active protocol once at startup through
//! [`set_inline_images`], so tests, `--json`, piped output and dripw keep the
//! plain `[N images attached]` marker. `DRIP_IMAGE_PROTOCOL=kitty|iterm2|none`
//! forces the choice.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use regex::Regex;

use crate::watch::ansi::c;

/// kitty caps one escape's base64 payload at 4096 bytes.
pub const KITTY_CHUNK_BYTES: usize = 4096;

/// Widest image, in terminal columns, an inline row will ask for.
pub const MAX_IMAGE_COLUMNS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

impl ImageProtocol {
    pub fn name(self) -> &'static str {
        match self {
            ImageProtocol::Kitty => "kitty",
            ImageProtocol::Iterm2 => "iterm2",
        }
    }
}

static ACTIVE: Mutex<Option<ImageProtocol>> = Mutex::new(None);

/// Select the protocol the renderers may use; `None` disables inline output
/// and leaves the text marker as the only representation.
pub fn set_inline_images(protocol: Option<ImageProtocol>) {
    if let Ok(mut slot) = ACTIVE.lock() {
        *slot = protocol;
    }
}

/// The protocol inline rows may use right now (`None` = marker only).
pub fn inline_images_protocol() -> Option<ImageProtocol> {
    ACTIVE.lock().ok().and_then(|slot| *slot)
}

/// Pure detection: `lookup` reads an environment variable and `is_tty` says
/// whether stdout is an interactive terminal. A non-tty stdout always wins
/// (redirected output must stay text), then an explicit
/// `DRIP_IMAGE_PROTOCOL` override, then the terminal's own announcements.
pub fn detect_image_protocol_from(
    lookup: &dyn Fn(&str) -> Option<String>,
    is_tty: bool,
) -> Option<ImageProtocol> {
    if !is_tty {
        return None;
    }

    if let Some(choice) = lookup("DRIP_IMAGE_PROTOCOL") {
        return match choice.trim().to_ascii_lowercase().as_str() {
            "kitty" | "ghostty" => Some(ImageProtocol::Kitty),
            "iterm2" | "iterm" | "wezterm" => Some(ImageProtocol::Iterm2),
            // "none", empty and anything unrecognised fall back to text.
            _ => None,
        };
    }

    let term = lookup("TERM").unwrap_or_default().to_ascii_lowercase();
    let term_program = lookup("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();

    // kitty graphics protocol: kitty speaks it natively and ghostty
    // implements it; WezTerm's support is partial, so WezTerm is handled
    // below with iTerm2 images instead.
    if term.contains("kitty")
        || lookup("KITTY_WINDOW_ID").is_some()
        || term_program == "ghostty"
        || lookup("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return Some(ImageProtocol::Kitty);
    }

    if term_program == "iterm.app" || term_program == "wezterm" || lookup("WEZTERM_PANE").is_some()
    {
        return Some(ImageProtocol::Iterm2);
    }

    None
}

/// Detect the protocol for this process (stdout must be a terminal).
pub fn detect_image_protocol() -> Option<ImageProtocol> {
    detect_image_protocol_from(
        &|key| std::env::var(key).ok(),
        std::io::stdout().is_terminal(),
    )
}

/// kitty's transmit-and-display escape for a PNG `columns` wide. The payload
/// is chunked exactly as the protocol requires: `m=1` announces another
/// chunk, `m=0` ends the upload; only the first chunk repeats the action and
/// format keys.
pub fn kitty_graphics_escape(png_base64: &str, columns: usize) -> String {
    let columns = columns.clamp(1, MAX_IMAGE_COLUMNS);
    let bytes = png_base64.as_bytes();

    if bytes.is_empty() {
        return format!("\x1b_Ga=T,f=100,m=0,c={columns};\x1b\\");
    }

    let total = (bytes.len() + KITTY_CHUNK_BYTES - 1) / KITTY_CHUNK_BYTES;
    let mut out = String::new();

    for (index, chunk) in bytes.chunks(KITTY_CHUNK_BYTES).enumerate() {
        let more = usize::from(index + 1 < total);
        let chunk = std::str::from_utf8(chunk).expect("base64 payload is ASCII");

        if index == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,m={more},c={columns};{chunk}\x1b\\"
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }

    out
}

/// iTerm2 inline-image escape: the file name travels base64 encoded, the
/// payload verbatim, and the width hint keeps the image inside the frame.
pub fn iterm2_inline_escape(base64_payload: &str, file_name: &str, columns: usize) -> String {
    let columns = columns.clamp(1, MAX_IMAGE_COLUMNS);
    let name = BASE64_STANDARD.encode(file_name.as_bytes());
    format!(
        "\x1b]1337;File=name={name};inline=1;width={columns};preserveAspectRatio=1:{base64_payload}\x07"
    )
}

struct Loaded {
    base64: String,
    name: String,
    png: bool,
}

/// The PNG signature, which is what lets kitty's `f=100` decode a payload.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Resolve one attachment to a base64 payload. Accepts a filesystem path or
/// a `data:image/...;base64,...` URL; anything unreadable yields `None`, so
/// a missing file degrades to the text marker instead of an error.
fn load(image: &str) -> Option<Loaded> {
    if let Some(rest) = image.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        if !meta.contains("base64") {
            return None;
        }
        let base64: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
        if base64.is_empty() {
            return None;
        }
        let png = meta.starts_with("image/png");
        return Some(Loaded {
            base64,
            name: if png { "image.png" } else { "image" }.to_string(),
            png,
        });
    }

    let bytes = std::fs::read(image).ok()?;
    let name = Path::new(image)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());

    Some(Loaded {
        png: bytes.starts_with(&PNG_MAGIC),
        base64: BASE64_STANDARD.encode(&bytes),
        name,
    })
}

/// One inline row per displayable image. Unreadable files are skipped, and
/// kitty only accepts PNG (its `f=100` format) — everything else stays a
/// text marker rather than a corrupt frame.
pub fn image_rows(images: &[String], columns: usize, protocol: ImageProtocol) -> Vec<String> {
    let mut rows = Vec::new();

    for image in images {
        let Some(loaded) = load(image) else { continue };

        match protocol {
            ImageProtocol::Kitty if loaded.png => {
                rows.push(kitty_graphics_escape(&loaded.base64, columns));
            }
            ImageProtocol::Kitty => continue,
            ImageProtocol::Iterm2 => {
                rows.push(iterm2_inline_escape(&loaded.base64, &loaded.name, columns));
            }
        }
    }

    rows
}

/// Inline rows for the terminal this process runs on: empty unless the TUI
/// enabled a protocol at startup.
pub fn inline_image_rows(images: &[String], columns: usize) -> Vec<String> {
    match inline_images_protocol() {
        Some(protocol) => image_rows(images, columns, protocol),
        None => Vec::new(),
    }
}

/// The whole goal-attachment block: the `[N images attached]` marker (the
/// only row when no protocol is active) followed by the inline images.
pub fn goal_image_rows(images: &[String], columns: usize) -> Vec<String> {
    let count = images.len();
    let plural = if count == 1 { "" } else { "s" };

    let mut rows = vec![c::gray(&format!("  [{count} image{plural} attached]"))];
    rows.extend(inline_image_rows(images, columns));
    rows
}

fn image_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"!\[[^\]]*\]\(([^)\s]+)\)").expect("valid image-link regex"))
}

/// Local paths of markdown image links in model text (`![alt](shot.png)`),
/// in first-seen order. Remote URLs are left alone: fetching them during a
/// repaint would block the UI.
pub fn markdown_image_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for capture in image_link_re().captures_iter(text) {
        let target = capture[1].trim_matches(|ch| ch == '"' || ch == '\'');

        if target.starts_with("http://") || target.starts_with("https://") || target.is_empty() {
            continue;
        }

        if !out.iter().any(|seen| seen == target) {
            out.push(target.to_string());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        let text = path.to_string_lossy().into_owned();
        (dir, text)
    }

    #[test]
    fn detection_requires_a_tty_then_follows_the_terminal() {
        let kitty = env(&[("TERM", "xterm-kitty")]);
        assert_eq!(detect_image_protocol_from(&kitty, false), None);
        assert_eq!(
            detect_image_protocol_from(&kitty, true),
            Some(ImageProtocol::Kitty)
        );

        assert_eq!(
            detect_image_protocol_from(&env(&[("TERM_PROGRAM", "iTerm.app")]), true),
            Some(ImageProtocol::Iterm2)
        );
        assert_eq!(
            detect_image_protocol_from(&env(&[("TERM_PROGRAM", "WezTerm")]), true),
            Some(ImageProtocol::Iterm2)
        );
        assert_eq!(
            detect_image_protocol_from(&env(&[("TERM_PROGRAM", "ghostty")]), true),
            Some(ImageProtocol::Kitty)
        );
        // Plain 256-color terminals announce neither protocol.
        assert_eq!(
            detect_image_protocol_from(&env(&[("TERM", "xterm-256color")]), true),
            None
        );
    }

    #[test]
    fn explicit_override_wins_and_none_disables() {
        let forced = env(&[("DRIP_IMAGE_PROTOCOL", "iterm2"), ("TERM", "xterm-kitty")]);
        assert_eq!(
            detect_image_protocol_from(&forced, true),
            Some(ImageProtocol::Iterm2)
        );

        let off = env(&[("DRIP_IMAGE_PROTOCOL", "none"), ("TERM", "xterm-kitty")]);
        assert_eq!(detect_image_protocol_from(&off, true), None);
    }

    #[test]
    fn kitty_payload_is_chunked_at_the_protocol_limit() {
        let payload = "A".repeat(KITTY_CHUNK_BYTES * 2 + 12);
        let escape = kitty_graphics_escape(&payload, 40);

        assert_eq!(escape.matches("\x1b_G").count(), 3);
        assert_eq!(escape.matches("\x1b\\").count(), 3);
        assert!(escape.starts_with("\x1b_Ga=T,f=100,m=1,c=40;"));

        // Reassemble: every chunk after the first drops the action/format
        // keys, and only the last one closes the upload with m=0.
        let chunks: Vec<&str> = escape
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .collect();
        let mut rebuilt = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let (head, body) = chunk.split_once(';').expect("chunk carries a control part");
            assert!(
                head.contains(&format!("m={}", usize::from(index + 1 < 3))),
                "{head}"
            );
            assert!(body.len() <= KITTY_CHUNK_BYTES, "chunk {} too long", index);
            rebuilt.push_str(body);
        }
        assert_eq!(rebuilt, payload);
    }

    #[test]
    fn iterm2_payload_round_trips_the_image_bytes() {
        let bytes = b"not-really-a-png but opaque to the terminal";
        let payload = BASE64_STANDARD.encode(bytes);
        let escape = iterm2_inline_escape(&payload, "shot.png", 24);

        assert!(escape.starts_with("\x1b]1337;File=name="));
        assert!(escape.ends_with('\x07'));
        assert!(escape.contains(";inline=1;width=24;preserveAspectRatio=1:"));

        // The name is base64 of the file name and the payload decodes to the
        // exact bytes on disk.
        let header = escape.trim_start_matches("\x1b]1337;File=name=");
        let (name, rest) = header.split_once(';').unwrap();
        assert_eq!(BASE64_STANDARD.decode(name).unwrap(), b"shot.png");
        let body = rest.split(':').nth(1).unwrap().trim_end_matches('\x07');
        assert_eq!(BASE64_STANDARD.decode(body).unwrap(), bytes);
    }

    #[test]
    fn image_rows_pick_a_format_per_protocol_and_skip_missing_files() {
        let png = [&PNG_MAGIC[..], b"fake png body"].concat();
        let (_guard, png_path) = write_temp("shot.png", &png);
        let (_text_guard, text_path) = write_temp("notes.txt", b"plain text");
        let images = vec![
            png_path.clone(),
            text_path.clone(),
            "/nope/missing.png".to_string(),
        ];

        let kitty = image_rows(&images, 40, ImageProtocol::Kitty);
        assert_eq!(kitty.len(), 1, "only the PNG is displayable by kitty");
        assert!(kitty[0].starts_with("\x1b_Ga=T,f=100,m=0,c=40;"));

        // The file name travels base64 encoded inside the escape, so compare
        // it after decoding rather than by substring.
        let name_of = |escape: &str| -> String {
            let header = escape.trim_start_matches("\x1b]1337;File=name=");
            let (name, _) = header.split_once(';').unwrap();
            String::from_utf8(BASE64_STANDARD.decode(name).unwrap()).unwrap()
        };

        let iterm = image_rows(&images, 40, ImageProtocol::Iterm2);
        assert_eq!(
            iterm.len(),
            2,
            "iTerm2 takes both real files, not the missing one"
        );
        assert_eq!(name_of(&iterm[0]), "shot.png");
        assert_eq!(name_of(&iterm[1]), "notes.txt");
    }

    #[test]
    fn goal_block_keeps_the_text_marker_when_disabled_and_adds_rows_when_on() {
        let png = [&PNG_MAGIC[..], b"body"].concat();
        let (_guard, png_path) = write_temp("shot.png", &png);
        let images = vec![png_path];

        // Nothing but the marker until the TUI opts in (the default in tests,
        // headless runs and dripw).
        set_inline_images(None);
        let off = goal_image_rows(&images, 40);
        assert_eq!(off.len(), 1);
        assert!(off[0].contains("[1 image attached]"));

        set_inline_images(Some(ImageProtocol::Iterm2));
        let on = goal_image_rows(&images, 40);
        assert_eq!(on.len(), 2);
        assert!(on[0].contains("[1 image attached]"));
        assert!(on[1].contains("\x1b]1337;File="));

        set_inline_images(None);
        assert_eq!(inline_images_protocol(), None);
    }

    #[test]
    fn markdown_image_links_resolve_locally_only() {
        let text = "see ![shot](shot.png) and ![b](https://x.example/y.png) and ![a](shot.png)";
        assert_eq!(markdown_image_paths(text), vec!["shot.png".to_string()]);
        assert!(markdown_image_paths("no images here").is_empty());
    }
}
