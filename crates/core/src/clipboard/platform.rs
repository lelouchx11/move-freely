//! OS-native clipboard readers and writers supporting plain text, Unicode,
//! HTML, RTF, PNG images, and file lists (`NSURL`, `CF_HDROP`, `text/uri-list`).

use std::borrow::Cow;
use std::io::Cursor;
use std::path::PathBuf;

use anyhow::Result;
use image::{ImageFormat, RgbaImage};

use super::ClipboardPayload;

pub trait ClipboardBackend: Send {
    /// Cheap monotonic change token (if available) to avoid reading heavy payloads
    /// when the clipboard hasn't changed.
    fn change_token(&mut self) -> Option<u64>;
    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>>;
    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()>;
}

pub fn new_platform_clipboard() -> Result<Box<dyn ClipboardBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(MacClipboard::new()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(WinClipboard::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(LinuxClipboard::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Ok(Box::new(MemoryClipboard::default()))
    }
}

/// In-memory clipboard backend used for automated tests without touching the host OS clipboard.
#[derive(Default)]
pub struct MemoryClipboard {
    seq: u64,
    current: Option<ClipboardPayload>,
}

impl MemoryClipboard {
    pub fn set_simulated(&mut self, payload: ClipboardPayload) {
        self.seq += 1;
        self.current = Some(payload);
    }

    pub fn current(&self) -> Option<&ClipboardPayload> {
        self.current.as_ref()
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn change_token(&mut self) -> Option<u64> {
        Some(self.seq)
    }

    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>> {
        Ok(self.current.clone())
    }

    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()> {
        self.seq += 1;
        self.current = Some(payload.clone());
        Ok(())
    }
}

fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let img = RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| anyhow::anyhow!("invalid RGBA dimensions {width}x{height}"))?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

fn decode_png_to_rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let dyn_img = image::load_from_memory_with_format(png, ImageFormat::Png)?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((w, h, rgba.into_raw()))
}

// ---------------------------------------------------------------------------
// macOS NSPasteboard + arboard hybrid
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct MacClipboard {
    arboard: arboard::Clipboard,
}

#[cfg(target_os = "macos")]
impl MacClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self {
            arboard: arboard::Clipboard::new()?,
        })
    }

    fn pb_string(type_name: &str) -> Option<String> {
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::NSString;
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        let ty = NSString::from_str(type_name);
        let s = unsafe { pb.stringForType(&ty) }?;
        Some(s.to_string())
    }

    fn pb_file_urls() -> Option<Vec<PathBuf>> {
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::{NSString, NSURL};
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        let items = unsafe { pb.pasteboardItems() }?;
        let file_url_type = NSString::from_str("public.file-url");
        let mut paths = Vec::new();
        let count = items.count();
        for i in 0..count {
            let item = unsafe { items.objectAtIndex(i) };
            if let Some(url_str) = unsafe { item.stringForType(&file_url_type) } {
                let s = url_str.to_string();
                if let Some(nsurl) = unsafe { NSURL::URLWithString(&NSString::from_str(&s)) } {
                    if let Some(path) = unsafe { nsurl.path() } {
                        let pb_path = PathBuf::from(path.to_string());
                        if pb_path.exists() {
                            paths.push(pb_path);
                        }
                    }
                }
            }
        }
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

#[cfg(target_os = "macos")]
impl ClipboardBackend for MacClipboard {
    fn change_token(&mut self) -> Option<u64> {
        use objc2_app_kit::NSPasteboard;
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        Some(unsafe { pb.changeCount() } as u64)
    }

    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>> {
        if let Some(paths) = Self::pb_file_urls() {
            return Ok(Some(ClipboardPayload::Files { paths }));
        }
        if let Ok(img) = self.arboard.get_image() {
            let png_bytes =
                encode_rgba_to_png(img.width as u32, img.height as u32, img.bytes.as_ref())?;
            return Ok(Some(ClipboardPayload::Image {
                png_bytes,
                width: img.width as u32,
                height: img.height as u32,
            }));
        }
        let html = Self::pb_string("public.html");
        let rtf = Self::pb_string("public.rtf");
        let plain = Self::pb_string("public.utf8-plain-text")
            .or_else(|| self.arboard.get_text().ok());
        if plain.is_some() || html.is_some() || rtf.is_some() {
            return Ok(Some(ClipboardPayload::Text {
                plain: plain.unwrap_or_default(),
                html,
                rtf,
            }));
        }
        Ok(None)
    }

    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()> {
        use objc2::msg_send;
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::{NSArray, NSString, NSURL};

        match payload {
            ClipboardPayload::Text { plain, html, rtf } => {
                if html.is_none() && rtf.is_none() {
                    self.arboard.set_text(plain)?;
                    return Ok(());
                }
                let pb = unsafe { NSPasteboard::generalPasteboard() };
                unsafe {
                    pb.clearContents();
                    pb.setString_forType(
                        &NSString::from_str(plain),
                        &NSString::from_str("public.utf8-plain-text"),
                    );
                    if let Some(h) = html {
                        pb.setString_forType(
                            &NSString::from_str(h),
                            &NSString::from_str("public.html"),
                        );
                    }
                    if let Some(r) = rtf {
                        pb.setString_forType(
                            &NSString::from_str(r),
                            &NSString::from_str("public.rtf"),
                        );
                    }
                }
            }
            ClipboardPayload::Image { png_bytes, .. } => {
                let (w, h, rgba) = decode_png_to_rgba(png_bytes)?;
                self.arboard.set_image(arboard::ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: Cow::Owned(rgba),
                })?;
            }
            ClipboardPayload::Files { paths } => {
                let pb = unsafe { NSPasteboard::generalPasteboard() };
                let mut urls = Vec::new();
                for p in paths {
                    let ns = NSString::from_str(&p.to_string_lossy());
                    let url = unsafe { NSURL::fileURLWithPath(&ns) };
                    urls.push(url);
                }
                let arr = NSArray::from_vec(urls);
                unsafe {
                    pb.clearContents();
                    let _: bool = msg_send![&*pb, writeObjects: &*arr];
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows Clipboard (`CF_HDROP`, `HTML Format`, `Rich Text Format`, `arboard`)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub struct WinClipboard {
    arboard: arboard::Clipboard,
}

#[cfg(target_os = "windows")]
impl WinClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self {
            arboard: arboard::Clipboard::new()?,
        })
    }
}

#[cfg(target_os = "windows")]
impl ClipboardBackend for WinClipboard {
    fn change_token(&mut self) -> Option<u64> {
        use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
        Some(unsafe { GetClipboardSequenceNumber() } as u64)
    }

    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>> {
        if let Some(paths) = win_read_hdrop() {
            return Ok(Some(ClipboardPayload::Files { paths }));
        }
        if let Ok(img) = self.arboard.get_image() {
            let png_bytes =
                encode_rgba_to_png(img.width as u32, img.height as u32, img.bytes.as_ref())?;
            return Ok(Some(ClipboardPayload::Image {
                png_bytes,
                width: img.width as u32,
                height: img.height as u32,
            }));
        }
        let html = win_read_registered_utf8("HTML Format").map(extract_cf_html_fragment);
        let rtf = win_read_registered_utf8("Rich Text Format");
        let plain = self.arboard.get_text().ok();
        if plain.is_some() || html.is_some() || rtf.is_some() {
            return Ok(Some(ClipboardPayload::Text {
                plain: plain.unwrap_or_default(),
                html,
                rtf,
            }));
        }
        Ok(None)
    }

    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()> {
        match payload {
            ClipboardPayload::Text { plain, html, rtf } => {
                if html.is_some() || rtf.is_some() {
                    win_write_rich_text(plain, html.as_deref(), rtf.as_deref())?;
                } else {
                    self.arboard.set_text(plain)?;
                }
            }
            ClipboardPayload::Image { png_bytes, .. } => {
                let (w, h, rgba) = decode_png_to_rgba(png_bytes)?;
                self.arboard.set_image(arboard::ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: Cow::Owned(rgba),
                })?;
            }
            ClipboardPayload::Files { paths } => {
                win_write_hdrop(paths)?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn win_read_hdrop() -> Option<Vec<PathBuf>> {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
            return None;
        }
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }
        let mut paths = Vec::new();
        if let Ok(HANDLE(ptr)) = GetClipboardData(CF_HDROP.0 as u32) {
            let hdrop = HDROP(ptr);
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None);
                if len > 0 {
                    let mut buf = vec![0u16; (len + 1) as usize];
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                    let p = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
                    if p.exists() {
                        paths.push(p);
                    }
                }
            }
        }
        let _ = CloseClipboard();
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

#[cfg(target_os = "windows")]
fn win_write_hdrop(paths: &[PathBuf]) -> Result<()> {
    use std::mem::size_of;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;

    let mut wide_list: Vec<u16> = Vec::new();
    for p in paths {
        wide_list.extend(p.to_string_lossy().encode_utf16());
        wide_list.push(0);
    }
    wide_list.push(0);
    let byte_len = size_of::<DROPFILES>() + wide_list.len() * 2;

    unsafe {
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, byte_len)?;
        let ptr = GlobalLock(hmem) as *mut u8;
        if ptr.is_null() {
            anyhow::bail!("GlobalLock failed for CF_HDROP");
        }
        let df = ptr as *mut DROPFILES;
        (*df).pFiles = size_of::<DROPFILES>() as u32;
        (*df).fWide = true.into();
        let dst = ptr.add(size_of::<DROPFILES>()) as *mut u16;
        std::ptr::copy_nonoverlapping(wide_list.as_ptr(), dst, wide_list.len());
        let _ = GlobalUnlock(hmem);

        OpenClipboard(HWND::default())?;
        let _ = EmptyClipboard();
        let res = SetClipboardData(CF_HDROP.0 as u32, HANDLE(hmem.0));
        let _ = CloseClipboard();
        res?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn win_read_registered_utf8(fmt_name: &str) -> Option<String> {
    use std::ffi::CStr;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    unsafe {
        let fmt = RegisterClipboardFormatW(&HSTRING::from(fmt_name));
        if fmt == 0 || IsClipboardFormatAvailable(fmt).is_err() {
            return None;
        }
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }
        let mut res = None;
        if let Ok(HANDLE(ptr)) = GetClipboardData(fmt) {
            let locked = GlobalLock(HGLOBAL(ptr)) as *const i8;
            if !locked.is_null() {
                res = CStr::from_ptr(locked).to_str().ok().map(|s| s.to_string());
                let _ = GlobalUnlock(HGLOBAL(ptr));
            }
        }
        let _ = CloseClipboard();
        res
    }
}

#[cfg(target_os = "windows")]
fn win_write_rich_text(plain: &str, html: Option<&str>, rtf: Option<&str>) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe fn set_global_bytes(fmt: u32, bytes: &[u8]) -> Result<()> {
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let ptr = GlobalLock(hmem) as *mut u8;
        if ptr.is_null() {
            anyhow::bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hmem);
        SetClipboardData(fmt, HANDLE(hmem.0))?;
        Ok(())
    }

    unsafe {
        OpenClipboard(HWND::default())?;
        let _ = EmptyClipboard();

        let wide: Vec<u16> = plain.encode_utf16().chain(std::iter::once(0)).collect();
        let wide_bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        let _ = set_global_bytes(CF_UNICODETEXT.0 as u32, wide_bytes);

        if let Some(h) = html {
            let fmt = RegisterClipboardFormatW(&HSTRING::from("HTML Format"));
            if fmt != 0 {
                let mut cf_html = format_cf_html(h).into_bytes();
                cf_html.push(0);
                let _ = set_global_bytes(fmt, &cf_html);
            }
        }
        if let Some(r) = rtf {
            let fmt = RegisterClipboardFormatW(&HSTRING::from("Rich Text Format"));
            if fmt != 0 {
                let mut bytes = r.as_bytes().to_vec();
                bytes.push(0);
                let _ = set_global_bytes(fmt, &bytes);
            }
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn format_cf_html(fragment: &str) -> String {
    let header_tmpl = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let prefix = "<html><body>\r\n<!--StartFragment-->";
    let suffix = "<!--EndFragment-->\r\n</body></html>";
    let start_html = header_tmpl.len();
    let start_frag = start_html + prefix.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + suffix.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n{prefix}{fragment}{suffix}"
    )
}

#[cfg(any(target_os = "windows", test))]
fn extract_cf_html_fragment(raw: String) -> String {
    if let (Some(s), Some(e)) = (
        raw.find("<!--StartFragment-->"),
        raw.find("<!--EndFragment-->"),
    ) {
        let start = s + "<!--StartFragment-->".len();
        if start <= e {
            return raw[start..e].to_string();
        }
    }
    raw
}

// ---------------------------------------------------------------------------
// Linux X11 + Wayland Clipboard
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub struct LinuxClipboard {
    arboard: Option<arboard::Clipboard>,
}

#[cfg(target_os = "linux")]
impl LinuxClipboard {
    pub fn new() -> Result<Self> {
        let arboard = arboard::Clipboard::new().ok();
        Ok(Self { arboard })
    }
}

#[cfg(target_os = "linux")]
impl ClipboardBackend for LinuxClipboard {
    fn change_token(&mut self) -> Option<u64> {
        None
    }

    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>> {
        if let Some(paths) = linux_read_file_uris() {
            return Ok(Some(ClipboardPayload::Files { paths }));
        }
        let Some(ab) = self.arboard.as_mut() else {
            return Ok(None);
        };
        if let Ok(img) = ab.get_image() {
            let png_bytes =
                encode_rgba_to_png(img.width as u32, img.height as u32, img.bytes.as_ref())?;
            return Ok(Some(ClipboardPayload::Image {
                png_bytes,
                width: img.width as u32,
                height: img.height as u32,
            }));
        }
        if let Ok(txt) = ab.get_text() {
            if !txt.is_empty() {
                return Ok(Some(ClipboardPayload::Text {
                    plain: txt,
                    html: None,
                    rtf: None,
                }));
            }
        }
        Ok(None)
    }

    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()> {
        match payload {
            ClipboardPayload::Text { plain, html, .. } => {
                if let Some(ab) = self.arboard.as_mut() {
                    if let Some(h) = html {
                        ab.set().html(h, Some(plain))?;
                    } else {
                        ab.set_text(plain)?;
                    }
                }
            }
            ClipboardPayload::Image { png_bytes, .. } => {
                if let Some(ab) = self.arboard.as_mut() {
                    let (w, h, rgba) = decode_png_to_rgba(png_bytes)?;
                    ab.set_image(arboard::ImageData {
                        width: w as usize,
                        height: h as usize,
                        bytes: Cow::Owned(rgba),
                    })?;
                }
            }
            ClipboardPayload::Files { paths } => {
                linux_write_file_uris(paths)?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_read_file_uris() -> Option<Vec<PathBuf>> {
    let out = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::process::Command::new("wl-paste")
            .args(["--no-newline", "--type", "text/uri-list"])
            .output()
            .ok()?
    } else {
        std::process::Command::new("xclip")
            .args(["-o", "-selection", "clipboard", "-t", "text/uri-list"])
            .output()
            .ok()?
    };
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut paths = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(raw) = trimmed.strip_prefix("file://") {
            let decoded = percent_decode(raw);
            let p = PathBuf::from(decoded);
            if p.exists() {
                paths.push(p);
            }
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

#[cfg(target_os = "linux")]
fn linux_write_file_uris(paths: &[PathBuf]) -> Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;
    let uri_list: String = paths
        .iter()
        .map(|p| format!("file://{}\r\n", p.to_string_lossy()))
        .collect();
    let mut cmd = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut c = std::process::Command::new("wl-copy");
        c.args(["--type", "text/uri-list"]);
        c
    } else {
        let mut c = std::process::Command::new("xclip");
        c.args(["-i", "-selection", "clipboard", "-t", "text/uri-list"]);
        c
    };
    if let Ok(mut child) = cmd.stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(uri_list.as_bytes());
        }
        let _ = child.wait();
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_round_trips_fragment() {
        let formatted = format_cf_html("<b>hello</b>");
        assert_eq!(extract_cf_html_fragment(formatted), "<b>hello</b>");
    }
}
