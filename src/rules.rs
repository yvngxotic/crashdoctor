use crate::model::{Confidence, Diagnosis, ModInfo, Report, StackFrame};

pub trait Rule {
    fn name(&self) -> &'static str;
    fn check(&self, r: &Report) -> Option<Diagnosis>;
}

pub struct Evaluation {
    pub diagnosis: Diagnosis,
    pub checks: Vec<(&'static str, bool)>,
}

// TODO: add a rule for duplicate mod ids, I still need a report with that exact text
pub fn rules() -> Vec<Box<dyn Rule>> {
    let mut all: Vec<Box<dyn Rule>> = vec![
        Box::new(OomRule),
        Box::new(MissingSymbolRule),
        Box::new(MixinRule),
        Box::new(ModLoadRule),
        Box::new(RegistryRule),
        Box::new(TickingRule),
    ];
    all.push(Box::new(LastResortRule));
    all
}

pub fn evaluate(r: &Report) -> Evaluation {
    let mut checks = Vec::new();
    let mut hit: Vec<Diagnosis> = Vec::new();
    for rule in rules() {
        match rule.check(r) {
            Some(d) => {
                checks.push((rule.name(), true));
                hit.push(d);
            }
            None => checks.push((rule.name(), false)),
        }
    }

    let mut chosen = None;
    for d in &hit {
        if d.confidence == Confidence::High {
            chosen = Some(d.clone());
            break;
        }
    }
    let diagnosis = chosen
        .or_else(|| hit.into_iter().next())
        .unwrap_or_else(|| guess_from_frames(r));
    Evaluation { diagnosis, checks }
}

pub fn diagnose(r: &Report) -> Diagnosis {
    evaluate(r).diagnosis
}

struct OomRule;

impl Rule for OomRule {
    fn name(&self) -> &'static str {
        "out of memory"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        let exc = r.exceptions.iter().find(|e| e.is("OutOfMemoryError"))?;
        let detail = exc
            .message
            .clone()
            .unwrap_or_else(|| "no message".to_string());
        let metaspace = detail.to_lowercase().contains("metaspace")
            || detail.to_lowercase().contains("compressed class");
        let current = r.xmx_mb.unwrap_or(2048);
        let target = if current < 4096 {
            4096
        } else if current < 8192 {
            8192
        } else {
            current.saturating_add(4096)
        };
        let cause = if metaspace {
            format!(
                "OutOfMemoryError ({detail}). The JVM ran out of Metaspace, the space that holds class metadata"
            )
        } else {
            format!(
                "OutOfMemoryError ({detail}). The heap hit its ceiling, so the game ran out of RAM"
            )
        };
        let mut d = Diagnosis::new(
            Some("no mod: the JVM ran out of memory".to_string()),
            cause,
            Confidence::High,
        );
        d.evidence.push(format!("exception: {}", exc.text()));
        if let Some(line) = &r.memory_line {
            d.evidence.push(format!("memory at crash: {line}"));
        }
        if r.mod_count() > 0 {
            d.evidence.push(format!("{} mods loaded", r.mod_count()));
        }
        match r.xmx_mb {
            Some(v) => d
                .evidence
                .push(format!("-Xmx now: {v} MB (from JVM Flags)")),
            None => d.evidence.push("no -Xmx in the JVM Flags".to_string()),
        }
        if metaspace {
            d.fix.push(
                "raise the class metadata space: -XX:MaxMetaspaceSize=1024m (or drop mods)"
                    .to_string(),
            );
            d.fix.push(
                "if it is already high, too many mods are generating classes: remove the ones you do not use"
                    .to_string(),
            );
        } else if current >= 6144 {
            d.fix.push(format!(
                "you already had {current}M of heap: hunt the leak (a mod holding chunks or entities) instead of raising it"
            ));
            d.fix.push(format!(
                "if you still want to try, go to {target}M with -Xmx{target}M"
            ));
        } else {
            d.fix.push(format!(
                "raise -Xmx from {current}M to {target}M (launcher -> Installations -> Edit -> JVM arguments)"
            ));
            d.fix.push(format!(
                "with that many mods, {target}M is a sane starting point for 1.20.1"
            ));
        }
        if metaspace {
            d.dev_tip.push("if you are a dev, Metaspace runs out when too many classes get generated at runtime. Look for runtime class generation, a huge annotation set, or two mods shipping the same library".to_string());
        } else {
            d.dev_tip.push(format!("if you are a dev, the heap size belongs to the pack. Only dig into your code if it still dies with {target}M or more, because then it is a leak: static maps keyed by Level or Entity, listeners or tasks you register per entity and never remove, and caches that never expire"));
            d.dev_tip.push("to see what holds the memory: jcmd <pid> GC.class_histogram, then jcmd <pid> GC.heap_dump dump.hprof and open the dump in MAT or IntelliJ".to_string());
        }
        Some(d)
    }
}

lazy_re!(
    re_symbol_call,
    r"(?:^|[\s(,])((?:[A-Za-z_$][\w$]*\.)+[A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$<>]*)\s*\("
);
lazy_re!(
    re_symbol_field,
    r"(?:^|[\s(,])((?:[A-Za-z_$][\w$]*\.)+[A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)\s*$"
);
lazy_re!(
    re_missing_class,
    r"(?:NoClassDefFoundError|ClassNotFoundException)[:\s]+([\w.$]+)"
);

#[rustfmt::skip]
const SYMBOL_ERRORS: [&str; 4] = ["NoSuchMethodError", "NoSuchFieldError", "NoClassDefFoundError", "ClassNotFoundException"];

struct MissingSymbolRule;

impl Rule for MissingSymbolRule {
    fn name(&self) -> &'static str {
        "missing symbol (NoSuchMethodError/NoSuchFieldError/NoClassDefFoundError)"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        let exc = r
            .exceptions
            .iter()
            .find(|e| SYMBOL_ERRORS.contains(&e.simple()))?;
        let kind = exc.simple().to_string();
        let msg = exc.message.clone().unwrap_or_default();
        let is_class = matches!(
            kind.as_str(),
            "NoClassDefFoundError" | "ClassNotFoundException"
        );
        let symbol = if is_class {
            missing_class(&msg).and_then(|name| parse_class(&name))
        } else {
            parse_symbol(&msg)
        };
        let frame = r.first_mod_frame();
        let culprit = frame.and_then(|f| r.mod_for_class(&f.class));
        let culprit_label = culprit.map(ModInfo::label);
        let owner_kind = symbol
            .as_ref()
            .map_or(OwnerKind::Unknown, |s| classify_owner(r, &s.owner));
        let mc = r
            .mc_version
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let loader = r.loader.label();
        let loader_v = r.loader_version.clone().unwrap_or_else(|| "?".to_string());
        let culprit_name = culprit_label
            .clone()
            .unwrap_or_else(|| "the mod in the stacktrace".to_string());
        let action = culprit
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "that mod".to_string());

        let cause = match (&owner_kind, is_class) {
            (OwnerKind::Forge, _) => {
                format!("{kind}: the mod asks for a {loader} symbol this build does not have")
            }
            (OwnerKind::Vanilla, _) => format!(
                "{kind}: the mod calls Minecraft code that {mc} does not have. This build is for another game version"
            ),
            (OwnerKind::OtherMod(other), true) => format!(
                "{kind}: {culprit_name} needs a class that {other} ships and that mod is not loaded (missing dependency)"
            ),
            (OwnerKind::OtherMod(other), false) => format!(
                "{kind}: {culprit_name} calls an API of {other} that this version no longer exposes (mod-to-mod mismatch)"
            ),
            (OwnerKind::Unknown, true) => format!(
                "{kind}: a class is missing from the classpath, and {culprit_name} expects another mod to provide it"
            ),
            (OwnerKind::Unknown, false) => format!(
                "{kind}: a symbol is missing from the classpath. {culprit_name} was compiled against another {loader}/Minecraft version"
            ),
        };
        let mut d = Diagnosis::new(culprit_label.clone(), cause, Confidence::High);

        match frame {
            Some(f) => d
                .evidence
                .push(format!("{} (stacktrace line {})", f.site(), f.index)),
            None => d.evidence.push(format!(
                "no mod frame in the stacktrace; the exception is: {}",
                exc.text()
            )),
        }
        match &symbol {
            Some(s) if is_class => d.evidence.push(format!(
                "missing class: {} ({})",
                s.describe(),
                owner_label(&owner_kind)
            )),
            Some(s) => {
                d.evidence.push(format!(
                    "missing symbol: {} ({})",
                    s.describe(),
                    owner_label(&owner_kind)
                ));
                d.evidence
                    .push(format!("signature the mod asks for: {}", s.signature));
            }
            None => d
                .evidence
                .push(format!("missing symbol: {}", truncate(&msg, 180))),
        }
        push_entry_and_env(&mut d, culprit, r);

        if d.culprit.is_none() {
            if let OwnerKind::OtherMod(id) = &owner_kind {
                d.culprit = r
                    .mod_by_id(id)
                    .map(ModInfo::label)
                    .or_else(|| Some(format!("{id} (not in this report's mod list)")));
            }
        }
        if culprit.is_none() && d.confidence == Confidence::High {
            d.confidence = Confidence::Medium;
        }

        match &owner_kind {
            OwnerKind::Forge => {
                d.fix.push(format!(
                    "update {loader} to the latest build for MC {mc} (you are on {loader_v}); the mod expects a newer API"
                ));
                d.fix.push(format!(
                    "or downgrade {action} to a build made for {loader} {loader_v}"
                ));
            }
            OwnerKind::Vanilla => {
                d.fix.push(format!(
                    "use the {action} build for MC {mc} (this one is compiled for another version)"
                ));
                d.fix.push(
                    "if you just moved the pack to another game version, that mod was left behind: check the rest the same way"
                        .to_string(),
                );
            }
            OwnerKind::OtherMod(other) => {
                let dep = r
                    .mod_by_id(other)
                    .map(ModInfo::label)
                    .unwrap_or(other.clone());
                d.fix
                    .push(format!("install {dep} on both client and server"));
                d.fix.push(format!(
                    "if it is already installed, the versions do not match: update {action} or {other} until they agree"
                ));
            }
            OwnerKind::Unknown => {
                d.fix.push(format!(
                    "find which mod provides that symbol and use its build for {loader} {loader_v} + MC {mc}"
                ));
                d.fix.push(
                    "if you compiled the mod yourself: rebuild it against the installed Forge"
                        .to_string(),
                );
            }
        }
        if let Some(f) = frame {
            match &symbol {
                Some(s) => d.dev_tip.push(format!(
                    "if you are a dev, open {} and read the call to {}: that symbol is not in this classpath",
                    f.site(),
                    s.describe()
                )),
                None => d.dev_tip.push(format!(
                    "if you are a dev, open {} and read the call that fails",
                    f.site()
                )),
            }
        }
        match &owner_kind {
            OwnerKind::Forge => d.dev_tip.push(format!(
                "the name or the argument list changed in a newer Forge: rebuild against {loader} {loader_v} (your gradle.properties), or raise the Forge dependency in mods.toml if you want the new API"
            )),
            OwnerKind::Vanilla => d.dev_tip.push(
                "that class was compiled for another Minecraft version: check the game version in gradle.properties and recompile".to_string(),
            ),
            OwnerKind::OtherMod(other) => d.dev_tip.push(format!(
                "pin the {other} version you compile against, ask its author for the new signature, or shade the API you need"
            )),
            OwnerKind::Unknown => d
                .dev_tip
                .push("diff the API you call against the version you compile against".to_string()),
        }
        if is_class {
            d.dev_tip.push(
                "for a missing class: declare that dependency in mods.toml (mandatory), or shade it into your jar with shadowJar and jarJar it".to_string(),
            );
        }
        if let Some(hint) = jar_hint(frame) {
            d.dev_tip.push(hint);
        }
        Some(d)
    }
}

struct Symbol {
    owner: String,
    name: String,
    signature: String,
    is_method: bool,
}

impl Symbol {
    fn describe(&self) -> String {
        if self.name.is_empty() {
            return self.owner.clone();
        }
        if self.is_method {
            format!("{}.{}()", self.owner, self.name)
        } else {
            format!("{}.{}", self.owner, self.name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerKind {
    Vanilla,
    Forge,
    OtherMod(String),
    Unknown,
}

fn parse_symbol(msg: &str) -> Option<Symbol> {
    let clean = msg.trim().trim_matches('\'').trim();
    if clean.is_empty() {
        return None;
    }
    let (owner, name, is_method) = if let Some(c) = re_symbol_call().captures(clean) {
        (c[1].to_string(), c[2].to_string(), true)
    } else {
        let c = re_symbol_field().captures(clean)?;
        (c[1].to_string(), c[2].to_string(), false)
    };
    Some(Symbol {
        owner,
        name,
        signature: truncate(clean, 180),
        is_method,
    })
}

fn parse_class(name: &str) -> Option<Symbol> {
    let clean = name.trim().trim_matches('\'').trim();
    if !clean.contains('.') {
        return None;
    }
    Some(Symbol {
        owner: clean.to_string(),
        name: String::new(),
        signature: clean.to_string(),
        is_method: false,
    })
}

fn missing_class(msg: &str) -> Option<String> {
    if let Some(c) = re_missing_class().captures(msg) {
        return Some(c[1].to_string());
    }
    let tokens: Vec<&str> = msg.split_whitespace().collect();
    tokens
        .iter()
        .rev()
        .find(|t| t.contains('.') && !t.starts_with('('))
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '$')
                .to_string()
        })
}

fn classify_owner(r: &Report, owner: &str) -> OwnerKind {
    if owner.starts_with("net.minecraft.") || owner.starts_with("com.mojang.") {
        return OwnerKind::Vanilla;
    }
    if owner.starts_with("net.minecraftforge.") || owner.starts_with("cpw.mods.") {
        return OwnerKind::Forge;
    }
    if let Some(m) = r.mod_for_class(owner) {
        return OwnerKind::OtherMod(m.id.clone());
    }
    OwnerKind::Unknown
}

fn owner_label(kind: &OwnerKind) -> String {
    match kind {
        OwnerKind::Vanilla => "Minecraft vanilla class".to_string(),
        OwnerKind::Forge => "Forge class".to_string(),
        OwnerKind::OtherMod(id) => format!("class of mod {id}"),
        OwnerKind::Unknown => "class from an unknown package".to_string(),
    }
}

lazy_re!(
    re_mixin_apply,
    r"(?m)Mixin apply for mod ([\w.\-]+) failed:\s*(.+)$"
);
lazy_re!(re_mixin_config, r"([\w.\-]+\.mixins?\.json)");
lazy_re!(re_mixin_arrow, r"([\w.$]+)\s*->\s*([\w.$]+)");
lazy_re!(re_mixin_hook, r"@(\w+)(?: annotation)? on ([\w$]+)");
lazy_re!(re_mixin_check, r"\((\d+)/(\d+)\) succeeded");

struct MixinRule;

impl Rule for MixinRule {
    fn name(&self) -> &'static str {
        "mixin apply failure"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        const MIXIN_TYPES: [&str; 8] = [
            "MixinTransformerError",
            "MixinApplyError",
            "InvalidMixinException",
            "MixinException",
            "InvalidInjectionException",
            "InjectionError",
            "MixinInitialisationError",
            "InvalidAccessorException",
        ];
        let by_exc = r
            .exceptions
            .iter()
            .find(|e| MIXIN_TYPES.contains(&e.simple()));
        let raw_hit = r.contains("Mixin apply")
            || r.contains("MixinTransformerError")
            || r.contains("MixinApplyError");
        if by_exc.is_none() && !raw_hit {
            return None;
        }

        let apply = re_mixin_apply().captures(&r.raw);
        let apply_mod = apply.as_ref().map(|c| c[1].to_string());
        let apply_rest = apply
            .as_ref()
            .and_then(|c| c.get(2))
            .map(|m| truncate(m.as_str().trim(), 220));
        let config = re_mixin_config().captures(&r.raw).map(|c| c[1].to_string());
        let mixin_class = re_mixin_arrow().captures(&r.raw).map(|c| c[1].to_string());
        let target_class = re_mixin_arrow().captures(&r.raw).map(|c| c[2].to_string());

        let culprit_id = apply_mod.clone().or_else(|| {
            config.as_deref().map(mod_id_from_config).or_else(|| {
                r.first_mod_frame()
                    .and_then(|f| r.mod_for_class(&f.class))
                    .map(|m| m.id.clone())
            })
        });
        let culprit = culprit_id.as_deref().and_then(|id| r.mod_by_id(id));
        let culprit_name = culprit
            .map(ModInfo::label)
            .or_else(|| culprit_id.clone())
            .unwrap_or_else(|| "an unknown mod".to_string());
        let who = culprit_id.clone().unwrap_or_else(|| "that mod".to_string());

        let target_txt = match target_class.as_deref() {
            Some(t) if t.starts_with("net.minecraft.") => format!("the vanilla class {t}"),
            Some(t) => t.to_string(),
            None => "a game class".to_string(),
        };
        let cause = format!(
            "Mixin apply failed: {who}'s mixin could not be applied to {target_txt}, and the injection no longer matches those targets. Either the Forge/MC build moved or another mod patches the same class"
        );
        let confidence = if culprit.is_none() {
            Confidence::Medium
        } else {
            Confidence::High
        };
        let mut d = Diagnosis::new(Some(culprit_name.clone()), cause, confidence);

        if let Some(cfg) = &config {
            d.evidence.push(format!("mixin config: {cfg}"));
        }
        if let (Some(mc), Some(tc)) = (&mixin_class, &target_class) {
            d.evidence.push(format!("mixin: {mc} -> {tc}"));
        }
        if let Some(c) = re_mixin_hook().captures(&r.raw) {
            match re_mixin_check().captures(&r.raw) {
                Some(chk) => d.evidence.push(format!(
                    "injection: @{} on {}() matched {}/{} targets",
                    &c[1], &c[2], &chk[1], &chk[2]
                )),
                None => d
                    .evidence
                    .push(format!("injection: @{} on {}()", &c[1], &c[2])),
            }
        }
        if let Some(rest) = &apply_rest {
            d.evidence.push(format!("error: {rest}"));
        } else if let Some(exc) = by_exc.or_else(|| r.root_cause()) {
            d.evidence
                .push(format!("error: {}", truncate(&exc.text(), 220)));
        }
        push_entry_and_env(&mut d, culprit, r);

        d.fix.push(format!(
            "update {who}: its mixin went stale against {target_txt}"
        ));
        d.fix.push(
            "if another mod also injects into that class, two mixins on the same method fight each other: boot without one of them"
                .to_string(),
        );
        d.fix.push(format!(
            "minimal test: {} + {who} alone, to confirm the crash is theirs",
            r.loader.label()
        ));
        match (&mixin_class, &target_class) {
            (Some(mc), Some(tc)) => d.dev_tip.push(format!(
                "if you are a dev, decompile {tc} and read the method {mc} injects into: the target moved or its signature changed, so your injection point has to follow it"
            )),
            _ => d.dev_tip.push(
                "if you are a dev, decompile the target class and read the method your mixin injects into: the injection point moved or the signature changed".to_string(),
            ),
        }
        d.dev_tip.push(
            "check the refmap inside the jar too. A stale refmap misses the target even when the method is there, and it is the usual reason behind 0 targets matched".to_string(),
        );
        if let Some(tc) = &target_class {
            if !tc.starts_with("net.minecraft.") {
                d.dev_tip.push(format!(
                    "the target {tc} is not vanilla, so another mod moved that method: pin the version of that mod, and tell its author if the API changed"
                ));
            }
        }
        Some(d)
    }
}

fn mod_id_from_config(config: &str) -> String {
    let mut name = config;
    for suffix in [".mixins.json", ".mixin.json", ".mixins", ".mixin", ".json"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped;
            break;
        }
    }
    for suffix in ["-common", "-client", "-server", "-main"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped;
            break;
        }
    }
    name.to_string()
}

lazy_re!(
    re_failure_message,
    r"(?m)^[ \t]*Failure message:\s*(.+?)\s*$"
);
lazy_re!(
    re_failure_detail,
    r"(?m)^[ \t]*Failure message:.*\n[ \t]+(\S.*?)\s*$"
);
lazy_re!(re_mod_file, r"(?m)^[ \t]*Mod File:\s*(.+?)\s*$");
lazy_re!(
    re_exception_message,
    r"(?m)^[ \t]*Exception message:\s*(.+?)\s*$"
);
lazy_re!(re_issue_url, r"(?m)^[ \t]*Mod Issue URL:\s*(.+?)\s*$");
lazy_re!(re_paren_id, r"\(([a-z][\w\-]*)\)");
lazy_re!(re_event_class, r"event class\s+([\w.$]+)");
lazy_re!(re_bus_iface, r"base type interface\s+([\w.$]+)");
lazy_re!(re_duplicate_mod, r"(?i)duplicate mod");
lazy_re!(
    re_missing_dependency,
    r"(?i)Mod ID:\s*'?([\w\-]+)'?,\s*Requested by:\s*'?([\w\-]+)'?(?:,\s*Expected range:\s*'?([^',]*)'?)?"
);
lazy_re!(
    re_requires,
    r"(?i)requires\s+([\w\-]+)\s*([\[\(][^\]\)]*[\]\)])?"
);

// TODO: 1.12 and 1.21 write the mod list in another layout, only 1.20.1 is handled here
struct ModLoadRule;

impl Rule for ModLoadRule {
    fn name(&self) -> &'static str {
        "mod loading failure"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        let failure = capture_of(re_failure_message(), &r.raw);
        let lower = r.raw.to_lowercase();
        let loading_failed = failure.is_some()
            || lower.contains("mod loading has failed")
            || lower.contains("mod loading error");
        if !loading_failed {
            return None;
        }

        let mod_file = capture_of(re_mod_file(), &r.raw);
        let mod_id = failure
            .as_deref()
            .and_then(|f| capture_of(re_paren_id(), f))
            .or_else(|| mod_file.as_deref().and_then(|f| mod_id_from_jar(f, r)));
        let entry = mod_id.as_deref().and_then(|id| r.mod_by_id(id));
        let blame = match entry.map(ModInfo::label) {
            Some(label) => label,
            None => mod_id.clone().unwrap_or_else(|| "a mod".to_string()),
        };

        let error_text = capture_of(re_exception_message(), &r.raw)
            .or_else(|| capture_of(re_failure_detail(), &r.raw))
            .or_else(|| failure.clone())
            .unwrap_or_else(|| "no error text".to_string());
        let failure_kind = classify_failure(&error_text);

        let cause = match &failure_kind {
            ModFailure::WrongBus { event, .. } => format!(
                "Mod loading failed: {blame} listens for {event} on the wrong event bus. That event belongs to the Forge bus, and this listener sits on the mod bus"
            ),
            ModFailure::Duplicate => {
                format!("Mod loading failed: {blame} is installed twice")
            }
            ModFailure::MissingDependency { dep, range } => {
                let want = range
                    .as_ref()
                    .map(|r| format!(" (version {r})"))
                    .unwrap_or_default();
                format!("Mod loading failed: {blame} needs {dep}{want} and it is not there")
            }
            ModFailure::MissingClass(class) => format!(
                "Mod loading failed: {blame} needs the class {class}, which is not in the modpack"
            ),
            ModFailure::Other(text) => format!(
                "Mod loading failed: {blame} blew up before the game got to the menu. {text}"
            ),
        };
        let mut confidence = Confidence::Medium;
        if mod_id.is_some() {
            confidence = Confidence::High;
        }
        let mut d = Diagnosis::new(Some(blame.clone()), cause, confidence);

        if let Some(file) = &mod_file {
            d.evidence.push(format!("mod file: {}", file_name(file)));
        }
        if let Some(line) = &failure {
            d.evidence.push(format!("Forge says: {line}"));
        }
        d.evidence
            .push(format!("error: {}", truncate(&error_text, 240)));
        if let Some(url) = capture_of(re_issue_url(), &r.raw) {
            if !url.eq_ignore_ascii_case("NOT PROVIDED") {
                d.evidence.push(format!("mod issue URL: {url}"));
            }
        }
        push_entry_and_env(&mut d, entry, r);

        let short = mod_id.clone().unwrap_or_else(|| blame.clone());
        match &failure_kind {
            ModFailure::WrongBus { event, iface } => {
                d.fix.push(format!(
                    "in code: register {event} on MinecraftForge.EVENT_BUS instead of the mod bus (interface {iface} means it is a Forge event)"
                ));
                d.fix.push(format!(
                    "in game: this is a bug in {short}. Use another build and report it; there is nothing you can fix on your side"
                ));
            }
            ModFailure::Duplicate => {
                d.fix
                    .push("delete the duplicate jar from your mods folder".to_string());
                d.fix.push(format!("keep only one build of {blame}"));
            }
            ModFailure::MissingDependency { dep, range } => {
                let want = range.as_ref().map(|r| format!(" {r}")).unwrap_or_default();
                d.fix.push(format!(
                    "install {dep}{want} on the client and on the server"
                ));
                d.fix
                    .push("the pair has to be on both sides with the same version".to_string());
            }
            ModFailure::MissingClass(class) => {
                match r.mod_for_class(class).map(ModInfo::label) {
                    Some(dep) => d.fix.push(format!(
                        "install {dep}: {blame} needs {class} and it is not loaded"
                    )),
                    None => d.fix.push(format!(
                        "install the mod or library that ships {class} (it is likely another mod {blame} depends on)"
                    )),
                }
                d.fix.push(
                    "if you compiled that mod yourself, the dependency is missing from your build"
                        .to_string(),
                );
            }
            ModFailure::Other(_) => {
                d.fix.push(format!(
                    "this is {blame}'s own failure: report it with this crash report attached"
                ));
                d.fix.push(format!(
                    "try another build of {blame} (or roll back to the previous one)"
                ));
            }
        }
        let loading_frame = r.first_mod_frame();
        match &failure_kind {
            ModFailure::WrongBus { event, .. } => {
                d.dev_tip.push(format!(
                    "if you are a dev, this is the bus mixup. Forge events like {event} go on MinecraftForge.EVENT_BUS or on @Mod.EventBusSubscriber(bus = Bus.FORGE). Mod lifecycle events go on the bus from FMLJavaModLoadingContext.get().getModEventBus()"
                ));
                if let Some(f) = loading_frame {
                    d.dev_tip
                        .push(format!("{} is the line that registers the listener", f.site()));
                }
            }
            ModFailure::Duplicate => d.dev_tip.push(
                "if you are a dev, check that your build output is not sitting in mods/ next to an older jar, and that you do not ship the same mod under two names".to_string(),
            ),
            ModFailure::MissingDependency { dep, .. } => d.dev_tip.push(format!(
                "if you are a dev, declare it in mods.toml: a [[dependencies.<your_modid>]] block with modId = \"{dep}\", mandatory = true and the version range you tested. Then the loader fails with a clear message instead of this crash"
            )),
            ModFailure::MissingClass(class) => d.dev_tip.push(format!(
                "if you are a dev, the class {class} ships in another artifact: declare it in mods.toml, or shade that library into your jar with shadowJar and jarJar it"
            )),
            ModFailure::Other(_) => d.dev_tip.push(
                "if you are a dev, decompile the mod named above and read its constructor and class static block: the error line Forge printed is the real one, the rest of the report is the wrapper".to_string(),
            ),
        }
        if let Some(hint) = jar_hint(loading_frame) {
            d.dev_tip.push(hint);
        }
        Some(d)
    }
}

enum ModFailure {
    WrongBus { event: String, iface: String },
    Duplicate,
    MissingDependency { dep: String, range: Option<String> },
    MissingClass(String),
    Other(String),
}

// TODO: duplicate mods are only caught by the "duplicate mod" text, the two jar paths Forge prints are not read yet
fn classify_failure(text: &str) -> ModFailure {
    let lower = text.to_lowercase();
    let bus_clash = lower.contains("imodbus") || lower.contains("not a subtype of the base type");
    if bus_clash {
        if let Some(c) = re_event_class().captures(text) {
            let iface =
                capture_of(re_bus_iface(), text).unwrap_or_else(|| "IModBusEvent".to_string());
            return ModFailure::WrongBus {
                event: c[1].to_string(),
                iface,
            };
        }
    }
    if re_duplicate_mod().is_match(text) {
        return ModFailure::Duplicate;
    }
    if let Some(c) = re_missing_dependency().captures(text) {
        return ModFailure::MissingDependency {
            dep: c[1].to_string(),
            range: c
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty()),
        };
    }
    if let Some(c) = re_requires().captures(text) {
        return ModFailure::MissingDependency {
            dep: c[1].to_string(),
            range: c
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty()),
        };
    }
    if let Some(class) = missing_class(text) {
        return ModFailure::MissingClass(class);
    }
    ModFailure::Other(truncate(text, 200))
}

fn mod_id_from_jar(jar: &str, r: &Report) -> Option<String> {
    let base = file_name(jar).trim_end_matches(".jar").to_string();
    let mut candidate = base;
    loop {
        if r.mod_by_id(&candidate).is_some() {
            return Some(candidate);
        }
        let cut = candidate.rfind('-')?;
        candidate.truncate(cut);
    }
}

fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

lazy_re!(
    re_registry_id,
    r"([a-z][a-z0-9_]{1,63}):([a-z0-9_./\-]{1,64})"
);
lazy_re!(re_mod_of_entry, r"(?:mod:\s*|from mod\s+)([\w\-]{2,64})");

struct RegistryRule;

impl Rule for RegistryRule {
    fn name(&self) -> &'static str {
        "registry mismatch"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        let marker = if r.contains("Missing registry entries") {
            "Missing registry entries"
        } else if r.contains("Failed to synchronize registry data") {
            "Failed to synchronize registry data"
        } else {
            return None;
        };
        let (ids, mod_ids) = missing_registry_ids(&r.raw, marker);
        let side = if r.contains("from server") || r.contains("from the server") {
            "the client"
        } else if r.contains("from client") || r.contains("from the client") {
            "the server"
        } else {
            "one of the two sides (client or server)"
        };

        let mut culprits: Vec<String> = Vec::new();
        for id in &ids {
            let ns = id.split(':').next().unwrap_or(id);
            if ns == "minecraft" {
                continue;
            }
            let label = r
                .mod_by_id(ns)
                .map(ModInfo::label)
                .unwrap_or_else(|| format!("{ns} (not in this report's mod list)"));
            if !culprits.contains(&label) {
                culprits.push(label);
            }
        }
        for id in &mod_ids {
            let label = r
                .mod_by_id(id)
                .map(ModInfo::label)
                .unwrap_or_else(|| format!("{id} (not in this report's mod list)"));
            if !culprits.contains(&label) {
                culprits.push(label);
            }
        }
        let culprit = if culprits.is_empty() {
            None
        } else {
            Some(culprits.join(", "))
        };
        let known = !ids.is_empty() || !mod_ids.is_empty();
        let confidence = if known {
            Confidence::High
        } else {
            Confidence::Medium
        };
        let ids_txt = if ids.is_empty() {
            "the registry entries the other side never registered".to_string()
        } else {
            preview(&ids, 8)
        };
        let cause = format!(
            "Missing registry entries: the client and the server are not running the same mods, and {side} is missing {ids_txt}"
        );
        let mut d = Diagnosis::new(culprit, cause, confidence);

        if let Some(line) = r.raw.lines().find(|l| l.contains(marker)) {
            d.evidence.push(format!("report line: {}", line.trim()));
        }
        if !ids.is_empty() {
            d.evidence
                .push(format!("missing IDs ({}): {}", ids.len(), preview(&ids, 8)));
        }
        if !mod_ids.is_empty() {
            d.evidence
                .push(format!("mods that register them: {}", mod_ids.join(", ")));
        }
        d.evidence.push(format!("environment: {}", r.env_summary()));

        if !mod_ids.is_empty() {
            d.fix
                .push(format!("install {} on {side}", mod_ids.join(", ")));
        } else if !ids.is_empty() {
            d.fix.push(format!(
                "install on {side} the mod that registers those IDs"
            ));
        }
        d.fix.push(
            "keep client and server on the same mod list and the same versions (compare them one by one)"
                .to_string(),
        );
        d.fix.push(
            "for a modpack server: resync the client's mods folder with the server's".to_string(),
        );
        if ids.is_empty() {
            d.dev_tip.push(
                "if you are a dev, that registry content has to exist on both sides: register it with DeferredRegister on the mod event bus, which runs on the client and on the server"
                    .to_string(),
            );
        } else {
            d.dev_tip.push(format!(
                "if you are a dev, the missing IDs are {}. Those come from a registry your mod fills: register with DeferredRegister on the mod event bus so both sides get them, and never add game content from a client-only event like FMLClientSetupEvent",
                preview(&ids, 6)
            ));
        }
        Some(d)
    }
}

fn missing_registry_ids(raw: &str, marker: &str) -> (Vec<String>, Vec<String>) {
    let Some(pos) = raw.find(marker) else {
        return (Vec::new(), Vec::new());
    };
    let tail = &raw[pos + marker.len()..];
    let mut ids: Vec<String> = Vec::new();
    let mut mods: Vec<String> = Vec::new();
    let mut lines = tail.lines();
    if let Some(first) = lines.next() {
        collect_ids(&format!("{marker}{first}"), &mut ids, &mut mods, true);
    }
    for line in lines.take(40) {
        let trimmed = line.trim();
        if trimmed.starts_with("at ") || trimmed.starts_with("--") || trimmed.starts_with("...") {
            break;
        }
        if trimmed.is_empty() {
            if ids.is_empty() {
                continue;
            }
            break;
        }
        collect_ids(trimmed, &mut ids, &mut mods, false);
    }
    (ids, mods)
}

fn collect_ids(line: &str, ids: &mut Vec<String>, mods: &mut Vec<String>, marker_line: bool) {
    let body = if marker_line {
        line.split_once("entries")
            .map(|(_, rest)| rest)
            .unwrap_or(line)
    } else {
        line
    };
    let target = match body.split_once("->") {
        Some((_, rhs)) => rhs.split('(').next().unwrap_or(rhs),
        None => body.split('(').next().unwrap_or(body),
    };
    for c in re_registry_id().captures_iter(target) {
        let id = format!("{}:{}", &c[1], &c[2]);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for c in re_mod_of_entry().captures_iter(body) {
        let id = c[1].to_string();
        if !mods.contains(&id) {
            mods.push(id);
        }
    }
}

lazy_re!(
    re_block_name,
    r"(?m)^[ \t]*Name:\s*([^\s/]+)\s*(?://\s*([^\s]+))?"
);
lazy_re!(
    re_block_type,
    r"(?m)^[ \t]*Block type:\s*([^\s(]+)(?:\s*\(([^)]+)\))?"
);
lazy_re!(
    re_entity_type,
    r"(?m)^[ \t]*Entity Type:\s*([^\s(]+)(?:\s*\(([^)]+)\))?"
);
lazy_re!(
    re_exact_location,
    r"(?m)^[ \t]*Entity's Exact location:\s*(-?[\d.]+,\s*-?[\d.]+,\s*-?[\d.]+)"
);
lazy_re!(
    re_block_location,
    r"World:\s*\(\s*(-?\d+),\s*(-?\d+),\s*(-?\d+)\)"
);

struct TickingRule;

impl Rule for TickingRule {
    fn name(&self) -> &'static str {
        "entity/block entity tick"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        let desc = r.description.clone().unwrap_or_default().to_lowercase();
        let entity_section = r.section("Entity being ticked").is_some();
        let block_section = r.section("Block entity being ticked").is_some();
        let marker = desc.contains("ticking")
            || entity_section
            || block_section
            || r.contains("Exception ticking entity")
            || r.contains("Exception ticking block entity");
        if !marker {
            return None;
        }

        let entity = re_entity_type().captures(&r.raw);
        let block_type = re_block_type().captures(&r.raw);
        let block_name = re_block_name().captures(&r.raw);
        let exact_pos = re_exact_location()
            .captures(&r.raw)
            .map(|c| c[1].to_string());
        let block_pos = re_block_location()
            .captures(&r.raw)
            .map(|c| format!("{},{},{}", &c[1], &c[2], &c[3]));

        let type_id = entity
            .as_ref()
            .map(|c| c[1].to_string())
            .or_else(|| block_name.as_ref().map(|c| c[1].to_string()))
            .or_else(|| block_type.as_ref().map(|c| c[1].to_string()));
        let class = entity
            .as_ref()
            .and_then(|c| c.get(2))
            .map(|m| m.as_str().to_string())
            .or_else(|| {
                block_name
                    .as_ref()
                    .and_then(|c| c.get(2))
                    .map(|m| m.as_str().to_string())
            })
            .or_else(|| {
                block_type
                    .as_ref()
                    .and_then(|c| c.get(2))
                    .map(|m| m.as_str().to_string())
            });
        let culprit = type_id
            .as_deref()
            .and_then(|t| r.mod_for_id_like(t))
            .or_else(|| class.as_deref().and_then(|c| r.mod_for_class(c)))
            .or_else(|| r.first_mod_frame().and_then(|f| r.mod_for_class(&f.class)));
        let culprit_name = culprit.map(ModInfo::label).or_else(|| {
            type_id
                .as_deref()
                .map(|t| format!("{t} (mod not identified)"))
        });
        let exc = r.root_cause();
        let frame = r.first_mod_frame();

        let is_block = block_section || block_type.is_some();
        let what = if is_block { "block entity" } else { "entity" };
        let subject = type_id.clone().unwrap_or_else(|| what.to_string());
        let cause = format!("Exception ticking {what}: {subject} blows up in its tick");
        let confidence = match culprit {
            Some(_) => Confidence::High,
            None => Confidence::Medium,
        };
        let mod_id = culprit
            .map(|m| m.id.clone())
            .unwrap_or_else(|| subject.clone());
        let mut d = Diagnosis::new(culprit_name, cause, confidence);

        match (&type_id, &class) {
            (Some(t), Some(c)) => d.evidence.push(format!("{what}: {t} ({c})")),
            (Some(t), None) => d.evidence.push(format!("{what}: {t}")),
            _ => d.evidence.push(format!(
                "the report talks about a {what} but I could not read its type"
            )),
        }
        if let Some(pos) = &exact_pos {
            match &block_pos {
                Some(bp) => d.evidence.push(format!("position: {pos} (block: {bp})")),
                None => d.evidence.push(format!("position: {pos}")),
            }
        } else if let Some(bp) = &block_pos {
            d.evidence.push(format!("position (block): {bp}"));
        }
        if let Some(e) = exc {
            d.evidence
                .push(format!("exception: {}", truncate(&e.text(), 200)));
        }
        if let Some(f) = frame {
            d.evidence.push(format!(
                "mod frame: {} (stacktrace line {})",
                f.site(),
                f.index
            ));
        }
        if let Some(m) = culprit {
            d.evidence.push(format!("mod entry: {}", m.detail()));
        }
        d.evidence.push(format!("environment: {}", r.env_summary()));

        d.fix.push(format!(
            "this is {mod_id}'s bug: report it with this crash report attached"
        ));
        if is_block {
            match &block_pos {
                Some(bp) => d.fix.push(format!(
                    "to get back in: break that block with /setblock {bp} air (or mine it in creative)"
                )),
                None => d.fix.push(
                    "to get back in: find that mod's block and remove it from the world"
                        .to_string(),
                ),
            }
        } else {
            match &exact_pos {
                Some(pos) => d.fix.push(format!(
                    "to get back in: kill the entity near that spot with /kill @e[type={},distance=64] (it was at {pos})",
                    type_id.clone().unwrap_or_else(|| "?".to_string())
                )),
                None => d.fix.push(
                    "to get back in: kill it with /kill @e[type=<the id above>]".to_string(),
                ),
            }
        }
        d.fix.push(format!(
            "try another build of {mod_id} (the previous one usually works)"
        ));
        match frame {
            Some(f) => d.dev_tip.push(format!(
                "if you are a dev, open {}: that is the line that throws",
                f.site()
            )),
            None => d.dev_tip.push(
                "if you are a dev, read the tick method of the class in the frames above"
                    .to_string(),
            ),
        }
        d.dev_tip.push(
            "fields you fill from the world (level, owner, block entity state) go null when the chunk unloads or the world changes, and the same happens when the entity is spawned by /summon or by a structure. Re-read them from the level every tick, or guard the tick with a null check"
                .to_string(),
        );
        if let Some(hint) = jar_hint(frame) {
            d.dev_tip.push(hint);
        }
        Some(d)
    }
}

struct LastResortRule;

impl Rule for LastResortRule {
    fn name(&self) -> &'static str {
        "no rule matched (package guess)"
    }

    fn check(&self, r: &Report) -> Option<Diagnosis> {
        Some(guess_from_frames(r))
    }
}

fn guess_from_frames(r: &Report) -> Diagnosis {
    let frame = r.first_own_frame();
    let owner = frame.and_then(|f| r.mod_for_class(&f.class));
    let blamed_exc = r
        .root_cause()
        .map(|e| e.simple().to_string())
        .unwrap_or_else(|| "no readable exception".to_string());

    let mut d = match owner {
        Some(m) => {
            let class = frame.map(|f| f.class.clone()).unwrap_or_default();
            let mut d = Diagnosis::new(
                Some(format!("{} (unconfirmed)", m.label())),
                format!(
                    "crashdoctor doesn't know (っ◞‸◟ c): no rule matched this crash, so the verdict comes from the package names in the stacktrace. {class} belongs to {}",
                    m.id
                ),
                Confidence::Low,
            );
            d.evidence.push(format!("mod entry: {}", m.detail()));
            d
        }
        None => Diagnosis::new(
            None,
            format!(
                "crashdoctor doesn't know (っ◞‸◟ c): no rule matched this crash, and no mod package shows up in the stacktrace. The exception is {blamed_exc}"
            ),
            Confidence::Low,
        ),
    };

    let mut seen: Vec<String> = Vec::new();
    let frames: Vec<&StackFrame> = r
        .frames
        .iter()
        .filter(|f| !f.is_platform())
        .filter(|f| {
            let site = f.site();
            if seen.contains(&site) {
                return false;
            }
            seen.push(site);
            true
        })
        .take(5)
        .collect();
    if frames.is_empty() {
        d.evidence
            .push("first frames of the stacktrace:".to_string());
        for f in r.frames.iter().take(5) {
            d.evidence.push(format!("  {}", f.site()));
        }
    } else {
        d.evidence.push(
            "first frames that are not Minecraft/Forge (the mod is usually there):".to_string(),
        );
        for f in frames {
            d.evidence
                .push(format!("  {} (stacktrace line {})", f.site(), f.index));
        }
    }
    if r.frames.is_empty() {
        d.evidence
            .push("this report has no readable stacktrace (truncated?)".to_string());
    }
    if let Some(e) = r.exceptions.first() {
        d.evidence.push(format!("exception: {}", e.text()));
    }
    d.evidence.push(format!("environment: {}", r.env_summary()));

    if owner.is_some() {
        d.fix.push(format!(
            "run {} plus that one mod alone to confirm it is the one crashing",
            r.loader.label()
        ));
        d.fix.push(
            "the frames above are what a mod author needs: open an issue with this crash report"
                .to_string(),
        );
        d.fix.push(
            "check for a newer build of that mod, this crash may already be fixed".to_string(),
        );
    } else {
        d.fix.push(
            "this looks like a vanilla/Forge crash: check your Java version and update Forge"
                .to_string(),
        );
        d.fix.push(
            "on a modpack, paste the report in the pack's support channel: the frames above are the only lead"
                .to_string(),
        );
    }
    if let Some(f) = frame {
        d.dev_tip.push(format!(
            "if you are a dev, open {}: that is the first frame of your code in the trace",
            f.site()
        ));
        d.dev_tip.push(
            "if the exception comes out of vanilla code, check what you pass into it: a null, an object from the other side (client versus server), or state read from an unloaded chunk"
                .to_string(),
        );
        if let Some(hint) = jar_hint(frame) {
            d.dev_tip.push(hint);
        }
    } else if !r.frames.is_empty() {
        d.dev_tip.push(
            "if you are a dev, there is no mod code in this stacktrace, so the crash is in vanilla or Forge. Compare the first frames with your Java version and search the exception text in the Forge issue tracker"
                .to_string(),
        );
    }
    d
}

fn push_entry_and_env(d: &mut Diagnosis, entry: Option<&ModInfo>, r: &Report) {
    if let Some(m) = entry {
        d.evidence.push(format!("mod entry: {}", m.detail()));
    }
    d.evidence.push(format!("environment: {}", r.env_summary()));
}

fn capture_of(re: &regex::Regex, text: &str) -> Option<String> {
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn jar_hint(frame: Option<&StackFrame>) -> Option<String> {
    let jar = frame?.jar.as_deref()?;
    if is_platform_jar(jar) {
        return None;
    }
    Some(format!(
        "to read that class: java -jar vineflower.jar -dgs=1 {jar} out/ (Vineflower, or FernFlower), or Ctrl+click the class in IntelliJ with the Minecraft Development plugin"
    ))
}

fn is_platform_jar(name: &str) -> bool {
    #[rustfmt::skip]
    const PLATFORM_JARS: [&str; 12] = [
        "client-", "server-", "forge-", "-srg", "authlib", "eventbus",
        "modlauncher", "javafml", "bootstraplauncher", "mixin-", "asm-",
        "lwjgl",
    ];
    let lower = name.to_lowercase();
    if lower.is_empty() {
        return false;
    }
    PLATFORM_JARS
        .iter()
        .any(|prefix| lower.starts_with(prefix) || lower.contains(prefix))
}

fn preview(items: &[String], max: usize) -> String {
    let shown: Vec<&str> = items.iter().take(max).map(String::as_str).collect();
    let mut txt = shown.join(", ");
    if items.len() > max {
        txt.push_str(&format!(" and {} more", items.len() - max));
    }
    txt
}

fn truncate(s: &str, max: usize) -> String {
    let taken: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{taken}...")
    } else {
        taken
    }
}
