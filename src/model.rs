use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    Forge,
    Fabric,
    NeoForge,
    Unknown,
}

impl Loader {
    #[rustfmt::skip]
    pub fn label(self) -> &'static str {
        match self {
            Loader::Forge    => "Forge",
            Loader::Fabric    => "Fabric",
            Loader::NeoForge => "NeoForge",
            Loader::Unknown  => "unknown loader",
        }
    }
}

#[derive(Debug)]
pub struct ModInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub file: Option<String>,
}

impl ModInfo {
    pub fn new(id: &str, name: &str, version: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            file: None,
        }
    }

    pub fn label(&self) -> String {
        format!("{} {}", self.id, self.version)
    }

    pub fn detail(&self) -> String {
        let file = self.file.as_deref().unwrap_or("");
        if file.is_empty() {
            return format!("{} {} ({})", self.id, self.version, self.name);
        }
        format!("{} {} ({}, {})", self.id, self.version, self.name, file)
    }
}

#[derive(Debug)]
pub struct StackFrame {
    pub index: usize,
    pub class: String,
    pub method: String,
    pub location: Option<String>,
    pub line: Option<u32>,
    pub jar: Option<String>,
}

impl StackFrame {
    pub fn site(&self) -> String {
        match &self.location {
            Some(loc) => format!("{}.{}({})", self.class, self.method, loc),
            None => format!("{}.{}()", self.class, self.method),
        }
    }

    pub fn is_platform(&self) -> bool {
        PLATFORM_PACKAGES.iter().any(|p| self.class.starts_with(*p))
    }
}

#[rustfmt::skip]
const PLATFORM_PACKAGES: [&str; 14] = [
    "java.", "javax.", "jdk.", "sun.", "com.sun.",
    "net.minecraft.", "net.minecraftforge.", "cpw.mods.", "com.mojang.",
    "org.spongepowered.", "org.lwjgl", "io.netty", "it.unimi.dsi",
    "org.apache.",
];

#[derive(Debug, PartialEq, Eq)]
pub struct Exc {
    pub fqcn: String,
    pub message: Option<String>,
    pub caused_by: bool,
}

impl Exc {
    pub fn simple(&self) -> &str {
        match self.fqcn.rsplit('.').next() {
            Some(name) => name,
            None => &self.fqcn,
        }
    }

    pub fn is(&self, simple: &str) -> bool {
        self.simple() == simple || self.fqcn == simple
    }

    pub fn text(&self) -> String {
        match &self.message {
            Some(msg) => format!("{}: {msg}", self.fqcn),
            None => self.fqcn.clone(),
        }
    }
}

#[derive(Debug)]
pub struct Section {
    pub name: String,
    pub body: String,
}

#[derive(Debug)]
pub struct Report {
    pub source: PathBuf,
    pub raw: String,
    pub loader: Loader,
    pub loader_version: Option<String>,
    pub description: Option<String>,
    pub mc_version: Option<String>,
    pub java_version: Option<String>,
    pub jvm_flags: Vec<String>,
    pub xmx_mb: Option<u32>,
    pub heap_max_mb: Option<u32>,
    pub memory_line: Option<String>,
    pub mods: Vec<ModInfo>,
    pub exceptions: Vec<Exc>,
    pub frames: Vec<StackFrame>,
    pub sections: Vec<Section>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn contains(&self, needle: &str) -> bool {
        self.raw.contains(needle)
    }

    pub fn section(&self, name: &str) -> Option<&str> {
        let needle = name.to_lowercase();
        self.sections
            .iter()
            .find(|s| s.name.to_lowercase().contains(&needle))
            .map(|s| s.body.as_str())
    }

    pub fn exception(&self, simple: &str) -> Option<&Exc> {
        self.exceptions.iter().find(|e| e.is(simple))
    }

    pub fn root_cause(&self) -> Option<&Exc> {
        self.exceptions
            .iter()
            .rfind(|e| e.caused_by)
            .or_else(|| self.exceptions.last())
    }

    pub fn mod_by_id(&self, id: &str) -> Option<&ModInfo> {
        let key = mod_id_key(id);
        self.mods.iter().find(|m| mod_id_key(&m.id) == key)
    }

    pub fn mod_for_id_like(&self, registry_id: &str) -> Option<&ModInfo> {
        let ns = registry_id.split(':').next().unwrap_or(registry_id);
        self.mod_by_id(ns)
    }

    pub fn mod_for_class(&self, class: &str) -> Option<&ModInfo> {
        let lower = class.to_lowercase();
        let tmp: Vec<&str> = lower.split('.').collect();
        let mut best: Option<&ModInfo> = None;
        for m in &self.mods {
            if is_platform_mod(&m.id) {
                continue;
            }
            let key = mod_id_key(&m.id);
            if key.len() < 3 || key.is_empty() {
                continue;
            }
            let hit = tmp.iter().any(|seg| mod_id_key(seg) == key) || lower.contains(&key);
            if hit && best.is_none_or(|b| mod_id_key(&b.id).len() < key.len()) {
                best = Some(m);
            }
        }
        best
    }

    pub fn first_mod_frame(&self) -> Option<&StackFrame> {
        self.frames
            .iter()
            .find(|f| !f.is_platform() && self.mod_for_class(&f.class).is_some())
    }

    pub fn first_own_frame(&self) -> Option<&StackFrame> {
        self.frames.iter().find(|f| !f.is_platform())
    }

    pub fn mod_count(&self) -> usize {
        let mut n = 0;
        for m in &self.mods {
            if !is_platform_mod(&m.id) {
                n += 1;
            }
        }
        n
    }

    pub fn env_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.loader == Loader::Unknown {
            parts.push("unknown loader".to_string());
        } else if let Some(v) = &self.loader_version {
            parts.push(self.loader.label().to_string() + " " + v);
        } else {
            parts.push(format!("{} (version unknown)", self.loader.label()));
        }
        if let Some(mc) = &self.mc_version {
            parts.push(format!("Minecraft {mc}"));
        }
        if let Some(java) = &self.java_version {
            parts.push(format!("Java {java}"));
        }
        parts.join(" · ")
    }
}

pub fn mod_id_key(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[rustfmt::skip]
pub fn is_platform_mod(id: &str) -> bool {
    matches!(
        mod_id_key(id).as_str(),
        "minecraft" | "forge" | "neoforge" | "neoforgefml" | "fabric" | "fabricloader"
            | "java" | "mcp" | "fml" | "modlauncher" | "mixin",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }

    #[rustfmt::skip]
    pub fn face(self) -> &'static str {
        match self {
            Confidence::High   => "( ദ്ദി ˙ᗜ˙ )",
            Confidence::Medium => "( ._. )",
            Confidence::Low    => "૮(˶ㅠ︿ㅠ)ა",
        }
    }
}

#[derive(Clone)]
pub struct Diagnosis {
    pub culprit: Option<String>,
    pub cause: String,
    pub evidence: Vec<String>,
    pub fix: Vec<String>,
    pub dev_tip: Vec<String>,
    pub confidence: Confidence,
}

impl Diagnosis {
    pub fn new(culprit: Option<String>, cause: String, confidence: Confidence) -> Self {
        Self {
            culprit,
            cause,
            evidence: Vec::new(),
            fix: Vec::new(),
            dev_tip: Vec::new(),
            confidence,
        }
    }
}
