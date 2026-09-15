use std::fs;
use std::path::{Path, PathBuf};

#[rustfmt::skip]
use crate::model::{
    mod_id_key, Exc, Loader, ModInfo, Report, Section, StackFrame,
};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("could not read \"{path}\": {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("\"{path}\" does not look like a Minecraft crash report: {missing}")]
    NotAReport { path: PathBuf, missing: String },
}

pub fn parse_file(path: &Path) -> Result<Report, ParseError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) => {
            return Err(ParseError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    parse(text, path)
}

pub fn parse(raw: &str, source: &Path) -> Result<Report, ParseError> {
    let sections = split_sections(raw);
    let exceptions = parse_exceptions(raw);
    let frames = parse_frames(raw);
    let description = capture1(re_description(), raw);
    let mods = parse_mods(raw);
    let loader = detect_loader(raw, &mods);
    let loader_version = loader_version(raw, &mods, loader);
    let mc_version = capture1(re_mc_version(), raw);
    let java_version = capture1(re_java(), raw);
    let jvm_flags = parse_jvm_flags(raw);
    let xmx_mb = jvm_flags.iter().find_map(|flag| parse_xmx(flag));
    let memory_line = capture1(re_memory(), raw);
    let heap_max_mb = memory_line.as_deref().and_then(parse_heap_max);

    if description.is_none()
        && exceptions.is_empty()
        && frames.is_empty()
        && mods.is_empty()
        && sections.is_empty()
    {
        return Err(ParseError::NotAReport {
            path: source.to_path_buf(),
            missing: "no \"Description:\", no stacktrace (\"at ...\"), no \"-- ... --\" sections \
                      and no mod list. Is this a complete crash report?"
                .to_string(),
        });
    }

    let mut warnings = Vec::new();
    if description.is_none() {
        warnings.push("no \"Description:\" line (truncated report?)".to_string());
    }
    if sections.is_empty() {
        warnings.push("no \"-- ... --\" sections at all".to_string());
    }
    if frames.is_empty() {
        warnings.push("no \"at ...\" frames: nothing to read in the stacktrace".to_string());
    }
    if mods.is_empty() {
        warnings.push("no mod list (\"Mod List:\"): cannot cross-reference versions".to_string());
    }
    if xmx_mb.is_none() {
        warnings.push("no \"-Xmx\" in the JVM Flags".to_string());
    }
    if re_system_details().find(raw).is_none() {
        warnings.push("no \"-- System Details --\" section".to_string());
    }
    if loader == Loader::Unknown {
        warnings.push("could not tell the loader apart (Forge, Fabric, NeoForge?)".to_string());
    }

    Ok(Report {
        source: source.to_path_buf(),
        raw: raw.to_string(),
        loader,
        loader_version,
        description,
        mc_version,
        java_version,
        jvm_flags,
        xmx_mb,
        heap_max_mb,
        memory_line,
        mods,
        exceptions,
        frames,
        sections,
        warnings,
    })
}

fn capture1(re: &regex::Regex, raw: &str) -> Option<String> {
    re.captures(raw)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
}

lazy_re!(re_description, r"(?m)^Description:\s*(.+?)\s*$");
lazy_re!(
    re_mc_version,
    r"(?m)^[ \t]*Minecraft Version:\s*([\w.\-+]+)\s*$"
);
lazy_re!(re_java, r"(?m)^[ \t]*Java Version:\s*(.+?)\s*$");
lazy_re!(
    re_jvm_flags,
    r"(?m)^[ \t]*JVM Flags:\s*\d+ total;\s*(.+?)\s*$"
);
lazy_re!(re_memory, r"(?m)^[ \t]*Memory:\s*(.+?)\s*$");
lazy_re!(re_heap_max, r"up to\s+\d+ bytes \((\d[\d,]*)\s*MiB\)");
lazy_re!(
    re_launched,
    r"(?m)^[ \t]*Launched Version:\s*(?:forge-|neoforge-)([\w.\-+]+)"
);
lazy_re!(
    re_forge_jar,
    r"(?i)forge-\d+\.\d+(?:\.\d+)?-(\d[\w.\-]*?)(?:-|\.jar)"
);
lazy_re!(re_system_details, r"(?m)^[ \t]*--\s*System Details\s*--");

lazy_re!(re_section, r"^[ \t]*--[ \t]*([^\-\s].*?)[ \t]*--[ \t]*$");

fn split_sections(raw: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    for line in raw.lines() {
        match re_section().captures(line) {
            Some(c) => {
                if let Some(s) = current.take() {
                    sections.push(s);
                }
                current = Some(Section {
                    name: c[1].to_string(),
                    body: String::new(),
                });
            }
            None => {
                if let Some(s) = current.as_mut() {
                    s.body.push_str(line);
                    s.body.push('\n');
                }
            }
        }
    }
    if let Some(s) = current {
        sections.push(s);
    }
    sections
}

lazy_re!(
    re_frame,
    r"^\s*at\s+(?:[\w.$]+/)?((?:[\w$]+\.)+[\w$]+)\.([\w$<>]+)\(([^)]*)\)(?:\s*~\[([^\]:]+\.jar))?"
);

fn parse_frames(raw: &str) -> Vec<StackFrame> {
    let mut frames: Vec<StackFrame> = Vec::new();
    for line in raw.lines() {
        if !line.contains("at ") {
            continue;
        }
        let Some(c) = re_frame().captures(line) else {
            continue;
        };
        let location = c
            .get(3)
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty());
        let line_no = location.as_deref().and_then(file_line);
        frames.push(StackFrame {
            index: frames.len() + 1,
            class: c[1].to_string(),
            method: c[2].to_string(),
            location,
            line: line_no,
            jar: c
                .get(4)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty()),
        });
    }
    frames
}

fn file_line(location: &str) -> Option<u32> {
    let (_, right) = location.rsplit_once(':')?;
    right.trim().parse().ok()
}

lazy_re!(
    re_exc,
    r"^((?:[\w$]+\.)+[A-Za-z_$][\w$]*(?:Exception|Error|Throwable|Failure))(?::\s*(.*))?$"
);

fn parse_exceptions(raw: &str) -> Vec<Exc> {
    let mut out: Vec<Exc> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        let (caused_by, rest) = match trimmed.strip_prefix("Caused by: ") {
            Some(r) => (true, r.trim()),
            None => (false, trimmed),
        };
        if rest.starts_with("at ") || rest.starts_with("...") {
            continue;
        }
        let Some(c) = re_exc().captures(rest) else {
            continue;
        };
        let exc = Exc {
            fqcn: c[1].to_string(),
            message: c
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty()),
            caused_by,
        };
        if !out.contains(&exc) {
            out.push(exc);
        }
    }
    out
}

lazy_re!(
    re_mod_header,
    r"(?m)^[ \t]*(?:Mod List|Mods|Forge Mod List|Loaded Mods)[ \t]*:[ \t]*$"
);
lazy_re!(re_kv, r"^[A-Za-z][A-Za-z0-9 _/.\-]*:\s*\S");
lazy_re!(re_paren_mod, r"^\(\s*([\w.\-+]+)\s+([\w.\-+]+)\s*\)$");

// TODO: 1.12 and 1.21 write the mod list in another layout, only 1.20.1 is handled here
fn parse_mods(raw: &str) -> Vec<ModInfo> {
    let mut out: Vec<ModInfo> = Vec::new();
    let mut in_list = false;
    for line in raw.lines() {
        if re_mod_header().is_match(line) {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        if re_section().is_match(line) {
            in_list = false;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if !is_mod_entry_line(line) {
            in_list = false;
            continue;
        }
        if let Some(m) = parse_mod_entry(line) {
            if !out.iter().any(|e| e.id == m.id) {
                out.push(m);
            }
        }
    }
    out
}

fn is_mod_entry_line(line: &str) -> bool {
    if line.contains('|') {
        return true;
    }
    let indented = line.starts_with('\t') || line.starts_with("  ");
    indented && !re_kv().is_match(line.trim())
}

fn parse_mod_entry(line: &str) -> Option<ModInfo> {
    let body = line.trim().trim_end_matches(',').trim();
    if body.is_empty() || body.starts_with("//") {
        return None;
    }
    if body.contains('|') {
        let fields: Vec<&str> = body.split('|').map(str::trim).collect();
        return match fields.as_slice() {
            [file, name, id, version, ..] if !id.is_empty() && !version.is_empty() => {
                let mut m = ModInfo::new(id, name, version);
                m.file = Some((*file).to_string()).filter(|s| !s.is_empty());
                Some(m)
            }
            [name, id, version] if !id.is_empty() && !version.is_empty() => {
                Some(ModInfo::new(id, name, version))
            }
            _ => None,
        };
    }
    if let Some(c) = re_paren_mod().captures(body) {
        return Some(ModInfo::new(&c[1], &c[1], &c[2]));
    }
    if re_kv().is_match(body) {
        return None;
    }
    let mut tokens = body.split_whitespace();
    let first = tokens.next()?;
    let id = first.strip_suffix(".jar").unwrap_or(first);
    let version = tokens.next()?;
    if !version.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(ModInfo::new(id, id, version))
}

fn detect_loader(raw: &str, mods: &[ModInfo]) -> Loader {
    if mods.iter().any(|m| mod_id_key(&m.id) == "neoforge") {
        return Loader::NeoForge;
    }
    if mods.iter().any(|m| mod_id_key(&m.id) == "forge") {
        return Loader::Forge;
    }
    let lower = raw.to_lowercase();
    if lower.contains("neoforge") || lower.contains("neo-forge") {
        return Loader::NeoForge;
    }
    if lower.contains("minecraftforge")
        || lower.contains("modlauncher")
        || lower.contains("forge-1.")
    {
        return Loader::Forge;
    }
    if lower.contains("fabricloader") || lower.contains("net.fabricmc") {
        return Loader::Fabric;
    }
    Loader::Unknown
}

fn loader_version(raw: &str, mods: &[ModInfo], loader: Loader) -> Option<String> {
    let keys: &[&str] = match loader {
        Loader::NeoForge => &["neoforge", "forge"],
        Loader::Forge => &["forge"],
        _ => &["forge", "neoforge", "fabricloader"],
    };
    for key in keys {
        let entry = mods
            .iter()
            .find(|m| mod_id_key(&m.id) == mod_id_key(key) && !m.version.is_empty());
        if let Some(m) = entry {
            return Some(m.version.clone());
        }
    }
    if let Some(v) = capture1(re_launched(), raw) {
        return Some(v);
    }
    re_forge_jar()
        .captures(raw)
        .map(|c| c[1].to_string())
        .filter(|v| v.chars().any(|c| c.is_ascii_digit()))
}

fn parse_jvm_flags(raw: &str) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let Some(line) = capture1(re_jvm_flags(), raw) else {
        return flags;
    };
    let parts: Vec<&str> = line.split_whitespace().collect();
    for part in parts {
        let tmp = part.trim();
        if tmp.starts_with('-') {
            flags.push(tmp.to_string());
        }
    }
    flags
}

lazy_re!(re_xmx, r"-Xmx(\d+)([kKmMgG]?)");

fn parse_xmx(flag: &str) -> Option<u32> {
    let c = re_xmx().captures(flag)?;
    let num: u32 = match c[1].parse() {
        Ok(v) => v,
        Err(_) => return None,
    };
    let mb = match c[2].to_ascii_lowercase().as_str() {
        "g" => num.saturating_mul(1024),
        "k" => num / 1024,
        "" => num / (1024 * 1024),
        _ => num,
    };
    if mb == 0 { None } else { Some(mb) }
}

// TODO: heap_max_mb only shows up in --debug, the OOM rule still ignores it
fn parse_heap_max(memory_line: &str) -> Option<u32> {
    let c = re_heap_max().captures(memory_line)?;
    c[1].replace(',', "").parse().ok()
}
