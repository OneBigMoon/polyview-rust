//! Display the official tdl process's QR on the desktop running that process.
use axum::{Router, http::header, response::Html, routing::get};
use std::{ffi::OsString, fmt::Write as _, io, net::Ipv4Addr, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    sync::watch,
    time::Instant,
};

// go-qrcode ToSmallString(false) uses light foreground blocks on a dark background.
// Each terminal row represents two rows of modules. The final lower half is padding.
fn svg(rows: &[String], size: usize) -> String {
    let mut path = String::new();
    for (row, line) in rows.iter().enumerate() {
        for (x, glyph) in line.chars().enumerate() {
            let halves = [matches!(glyph, ' ' | '▄'), matches!(glyph, ' ' | '▀')];
            for (half, black) in halves.into_iter().enumerate() {
                let y = row * 2 + half;
                if black && y < size {
                    write!(path, "M{x} {y}h1v1h-1z").unwrap();
                }
            }
        }
    }
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" role="img" aria-label="Telegram 登录二维码" width="330" height="330" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="white"/><path d="{path}"/></svg>"#
    )
}

type Frame = Option<(Instant, String)>;

#[derive(Default)]
struct Decoder {
    rows: Vec<String>,
    width: usize,
    line: Vec<u8>,
    skip_line: bool,
    escape: bool,
}

impl Decoder {
    fn push(&mut self, bytes: &[u8], frame: &watch::Sender<Frame>) {
        for &byte in bytes {
            if byte == 0x1b {
                self.escape = true;
                continue;
            }
            if self.escape {
                if byte != b'[' && (0x40..=0x7e).contains(&byte) {
                    self.escape = false;
                    if byte == b'K' {
                        frame.send_replace(None);
                        self.rows.clear();
                    }
                }
                continue;
            }
            match byte {
                b'\n' => {
                    if !self.skip_line {
                        self.finish_line(frame);
                    }
                    self.line.clear();
                    self.skip_line = false;
                }
                b'\r' => (),
                _ if self.skip_line => (),
                // Only retain bytes from the four possible UTF-8 glyphs, never prompt text.
                b' ' | 0xe2 | 0x96 | 0x80 | 0x84 | 0x88 if self.line.len() < 3 * 185 => {
                    self.line.push(byte)
                }
                _ => {
                    self.skip_line = true;
                    self.line.clear();
                    self.rows.clear();
                    frame.send_replace(None);
                }
            }
        }
    }

    fn finish_line(&mut self, frame: &watch::Sender<Frame>) {
        let Ok(line) = std::str::from_utf8(&self.line) else {
            self.rows.clear();
            return;
        };
        let width = line.chars().count();
        if !(29..=185).contains(&width)
            || width % 4 != 1
            || !line.chars().all(|c| matches!(c, '█' | '▀' | '▄' | ' '))
        {
            self.rows.clear();
            return;
        }
        if self.width != width {
            self.rows.clear();
            self.width = width;
        }
        if self.rows.is_empty() {
            if !line.chars().all(|c| c == '█') {
                return;
            }
            frame.send_replace(None);
        }
        self.rows.push(line.into());
        if self.rows.len() == width.div_ceil(2) {
            if line.chars().all(|c| c == '▀') {
                frame.send_replace(Some((Instant::now(), svg(&self.rows, width))));
            }
            self.rows.clear();
        }
    }
}

fn page(frame: &Frame) -> Html<String> {
    let body = frame
        .as_ref()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
        .map_or(
            "<p>等待宿主机上的 tdl 生成有效二维码。</p>",
            |(_, image)| image.as_str(),
        );
    Html(format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="refresh" content="1"><title>PolyView · Telegram 登录</title><main><h1>Telegram 登录</h1>{body}<p>请用自己的 Telegram 扫码。如需两步验证，请回到启动 tdl 的终端输入密码。</p></main></html>"#
    ))
}

pub async fn run(
    mut args: impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tool = args.next().ok_or("Missing tdl executable")?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let path = format!("/{}", uuid::Uuid::new_v4().simple());
    let url = format!("http://{}{path}", listener.local_addr()?);
    let (frames, current) = watch::channel(None);
    let app = Router::new().route(&path, get(move || {
        let html = page(&current.borrow());
        async move { ([(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; frame-ancestors 'none'; form-action 'none'; base-uri 'none'"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], html) }
    }));
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    eprintln!("请在本机浏览器打开 {url}");
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if let Ok(mut browser) = Command::new(opener)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            tokio::spawn(async move {
                let _ = browser.wait().await;
            });
        }
    }
    let mut output = child.stdout.take().expect("piped tdl stdout");
    let mut terminal = tokio::io::stdout();
    let mut decoder = Decoder::default();
    let mut buffer = [0; 4096];
    loop {
        let count = output.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        decoder.push(&buffer[..count], &frames);
        // Forward even partial prompts and cursor queries. Only tdl reads the terminal.
        terminal.write_all(&buffer[..count]).await?;
        terminal.flush().await?;
    }
    frames.send_replace(None);
    server.abort();
    if !child.wait().await?.success() {
        return Err(io::Error::other("tdl login did not complete").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smallest standard QR symbol including its quiet zone, with one test column.
    fn fixture(glyph: char) -> String {
        let blank = "█".repeat(29);
        let mut rows = vec![blank.clone(); 14];
        rows[3] = format!("{}{glyph}{}", "█".repeat(5), "█".repeat(23));
        rows.push("▀".repeat(29));
        rows.join("\n") + "\n"
    }

    #[test]
    fn fragmented_frames_rotate_and_clear_before_terminal_password_input() {
        let (frames, _) = watch::channel(None);
        let mut decoder = Decoder::default();
        for chunk in fixture('▄').as_bytes().chunks(2) {
            decoder.push(chunk, &frames);
        }
        let html = page(&frames.borrow()).0;
        assert!(html.contains("M5 6h1v1h-1z"));
        assert!(!html.contains("M5 7h1v1h-1z"));
        decoder.push(b"\x1b[A\x1b[A", &frames);
        decoder.push(fixture('▀').as_bytes(), &frames);
        let html = page(&frames.borrow()).0;
        assert!(html.contains("M5 7h1v1h-1z"));
        assert!(!html.contains("M5 6h1v1h-1z"));
        decoder.push(b"\x1b[KEnter 2FA Password: ", &frames);
        assert!(frames.borrow().is_none());
        assert!(!page(&frames.borrow()).0.contains("Password"));
    }

    #[test]
    fn incomplete_frames_and_expired_codes_are_hidden() {
        let (frames, _) = watch::channel(None);
        let mut decoder = Decoder::default();
        decoder.push(fixture(' ').trim_end().as_bytes(), &frames);
        assert!(frames.borrow().is_none());
        decoder.push(b"\n", &frames);
        let (_, image) = frames.borrow().clone().unwrap();
        assert!(
            !page(&Some((Instant::now() - Duration::from_secs(31), image)))
                .0
                .contains("<svg")
        );
    }
}
