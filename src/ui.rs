use std::cmp::Reverse;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::Diagnosis;
use crate::report;
use crate::rules;

const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_LISTED: usize = 25;
const PAGE: &str = include_str!("ui/panel.html");

pub struct UiOptions {
    pub port: u16,
    pub extra_dir: Option<PathBuf>,
    pub open_browser: bool,
}

struct UiState {
    token: String,
    dirs: Vec<PathBuf>,
    explicit_dirs: Vec<PathBuf>,
    port: u16,
}

pub fn serve(opts: UiOptions) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", opts.port))?;
    let port = listener.local_addr()?.port();
    let explicit_dirs: Vec<PathBuf> = opts.extra_dir.iter().cloned().collect();
    let state = Arc::new(UiState {
        token: make_token(),
        dirs: scan_dirs(&explicit_dirs),
        explicit_dirs,
        port,
    });

    let url = format!("http://127.0.0.1:{port}/?t={}", state.token);
    println!("crashdoctor ui: {url}");
    if opts.open_browser && !open_browser(&url) {
        println!("open that URL in your browser (could not launch one myself)");
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(err) = handle(stream, &state) {
                if let Some(peer) = peer {
                    eprintln!("ui: {peer}: {err}");
                }
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, state: &UiState) -> std::io::Result<()> {
    let req = match read_request(&mut stream) {
        Ok(req) => req,
        Err(err) => {
            return respond(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                &err.to_string(),
            );
        }
    };

    let own_host = format!("127.0.0.1:{}", state.port);
    let alias_host = format!("localhost:{}", state.port);
    let host_ok = match &req.host {
        Some(host) => host == &own_host || host == &alias_host,
        None => false,
    };
    if !host_ok {
        return respond(
            &mut stream,
            403,
            "text/plain; charset=utf-8",
            "bad Host header: this server only answers to 127.0.0.1",
        );
    }

    let path = req.target.split('?').next().unwrap_or("/").to_string();
    let main_token = req.token.as_deref() == Some(&state.token);
    let page_token = query_value(&req.target, "t").as_deref() == Some(&state.token);

    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") if page_token => {
            let page = PAGE.replace("__TOKEN__", &state.token);
            respond(&mut stream, 200, "text/html; charset=utf-8", &page)
        }
        ("GET", "/") => respond(
            &mut stream,
            403,
            "text/plain; charset=utf-8",
            "missing or wrong token: use the URL printed by crashdoctor",
        ),
        ("GET", "/api/recent") if main_token => {
            let body = recent_json(state);
            respond(&mut stream, 200, "application/json; charset=utf-8", &body)
        }
        ("POST", "/api/diagnose-text") if main_token => {
            let text = String::from_utf8_lossy(&req.body);
            let answer = diagnose_text(&text, "dropped report");
            respond(
                &mut stream,
                answer.status,
                "application/json; charset=utf-8",
                &answer.body,
            )
        }
        ("POST", "/api/diagnose-file") if main_token => {
            let asked = String::from_utf8_lossy(&req.body).trim().to_string();
            match allowed_path(&asked, state) {
                Ok(path) => {
                    let answer = match fs::read(&path) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                            let label = path.display().to_string();
                            diagnose_text(&text, &label)
                        }
                        Err(err) => {
                            Answer::bad(format!("could not read {}: {err}", path.display()))
                        }
                    };
                    respond(
                        &mut stream,
                        answer.status,
                        "application/json; charset=utf-8",
                        &answer.body,
                    )
                }
                Err(why) => respond(
                    &mut stream,
                    400,
                    "application/json; charset=utf-8",
                    &error_json(&why),
                ),
            }
        }
        (_, "/api/recent") | (_, "/api/diagnose-text") | (_, "/api/diagnose-file") => respond(
            &mut stream,
            403,
            "application/json; charset=utf-8",
            &error_json("missing or wrong token header"),
        ),
        _ => respond(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            "there is nothing here",
        ),
    }
}

struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn bad(why: String) -> Self {
        Self {
            status: 400,
            body: error_json(&why),
        }
    }
}

fn diagnose_text(text: &str, label: &str) -> Answer {
    let report = match report::parse(text, Path::new(label)) {
        Ok(report) => report,
        Err(err) => return Answer::bad(err.to_string()),
    };
    let evaluation = rules::evaluate(&report);
    let exit = if evaluation.diagnosis.culprit.is_some() {
        0
    } else {
        2
    };
    Answer {
        status: 200,
        body: diagnosis_json(&evaluation.diagnosis, label, exit),
    }
}

fn diagnosis_json(d: &Diagnosis, source: &str, exit: u8) -> String {
    let evidence: Vec<String> = d.evidence.iter().map(|line| json_str(line)).collect();
    let fix: Vec<String> = d.fix.iter().map(|line| json_str(line)).collect();
    let dev_tip: Vec<String> = d.dev_tip.iter().map(|line| json_str(line)).collect();
    format!(
        "{{\"source\":{},\"culprit\":{},\"cause\":{},\"evidence\":[{}],\"fix\":[{}],\
         \"dev_tip\":[{}],\"confidence\":{},\"face\":{},\"exit\":{},\"empty\":false}}",
        json_str(source),
        json_str(d.culprit.as_deref().unwrap_or("unknown")),
        json_str(&d.cause),
        evidence.join(","),
        fix.join(","),
        dev_tip.join(","),
        json_str(d.confidence.label()),
        json_str(d.confidence.face()),
        exit
    )
}

fn error_json(why: &str) -> String {
    format!("{{\"empty\":true,\"error\":{}}}", json_str(why))
}

fn json_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Request {
    method: String,
    target: String,
    host: Option<String>,
    token: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    if reader.read_line(&mut first)? == 0 {
        return Err(io_err("empty request"));
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut host = None;
    let mut token = None;
    let mut length = 0usize;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.to_ascii_lowercase().as_str() {
            "host" => host = Some(value.trim().to_string()),
            "x-crashdoctor-token" => token = Some(value.trim().to_string()),
            "content-length" => length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err(io_err("that request body is too big"));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request {
        method,
        target,
        host,
        token,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[rustfmt::skip]
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _   => "Error",
    }
}

fn io_err(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

fn query_value(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            return Some(value.to_string());
        }
    }
    None
}

fn scan_dirs(explicit: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        dirs.push(PathBuf::from(home).join(".minecraft").join("crash-reports"));
    }
    dirs.push(PathBuf::from("crash-reports"));
    dirs.push(PathBuf::from("."));
    dirs.extend(explicit.iter().cloned());
    dirs
}

struct Found {
    path: PathBuf,
    modified: SystemTime,
    description: String,
    size: u64,
}

// TODO: this re-reads every file in the folder on each GET, fine for crash-reports, painful on a 1 GB mods dir
fn recent_json(state: &UiState) -> String {
    let mut found: Vec<Found> = Vec::new();
    for dir in &state.dirs {
        let explicit = state.explicit_dirs.contains(dir);
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let wanted = name.ends_with(".txt") && (explicit || name.starts_with("crash-"));
            if !wanted {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            found.push(Found {
                description: peek_description(&path),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                size: meta.len(),
                path,
            });
        }
    }
    found.sort_by_key(|f| Reverse(f.modified));
    found.truncate(MAX_LISTED);

    let now = SystemTime::now();
    let mut items = Vec::new();
    for f in &found {
        let age = age_label(now, f.modified);
        items.push(format!(
            "{{\"path\":{},\"name\":{},\"size\":\"{}\",\"age\":{},\"description\":{}}}",
            json_str(&f.path.display().to_string()),
            json_str(&file_name(&f.path)),
            size_label(f.size),
            json_str(&age),
            json_str(&f.description)
        ));
    }
    format!("[{}]", items.join(","))
}

fn peek_description(path: &Path) -> String {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return String::new(),
    };
    let mut found: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Description:") {
            found = Some(rest.trim().chars().take(90).collect());
            break;
        }
    }
    found.unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn size_label(bytes: u64) -> String {
    if bytes == 0 {
        return "1 KB".to_string();
    }
    let tmp = bytes as f64 / (1024.0 * 1024.0);
    if bytes >= 1024 * 1024 {
        format!("{tmp:.1} MB")
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

fn age_label(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        return format!("{} min ago", secs / 60);
    }
    if secs < 86400 {
        return format!("{} h ago", secs / 3600);
    }
    let days = secs / 86400;
    if days == 1 {
        "yesterday".to_string()
    } else {
        format!("{days} days ago")
    }
}

fn allowed_path(asked: &str, state: &UiState) -> Result<PathBuf, String> {
    if asked.is_empty() {
        return Err("no path in the request".to_string());
    }
    let path = fs::canonicalize(asked).map_err(|err| format!("could not open {asked}: {err}"))?;
    for dir in &state.dirs {
        let Ok(root) = fs::canonicalize(dir) else {
            continue;
        };
        if path.starts_with(&root) {
            return Ok(path);
        }
    }
    Err(format!(
        "{} is outside the folders crashdoctor looks at",
        path.display()
    ))
}

fn make_token() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let mut state = (now as u64) ^ ((std::process::id() as u64) << 48);
    let mut token = String::with_capacity(16);
    for _ in 0..16 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        token.push(char::from_digit((state % 16) as u32, 16).unwrap_or('0'));
    }
    token
}

fn open_browser(url: &str) -> bool {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", ""])
    } else if cfg!(target_os = "macos") {
        ("open", vec![])
    } else {
        ("xdg-open", vec![])
    };
    let mut command = Command::new(program);
    command.args(args).arg(url);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
