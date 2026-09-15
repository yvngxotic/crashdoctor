use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use clap::error::ErrorKind;

use crashdoctor::model::{Confidence, Diagnosis, Loader, Report};
use crashdoctor::report;
use crashdoctor::rules::{self, Evaluation};
use crashdoctor::ui;

const ABOUT: &str = "Offline crash report doctor for Minecraft Java Edition (Forge 1.20.1)";

const LONG_ABOUT: &str = "Reads a Forge Minecraft crash report and tells you which mod broke the \
game, why, what the evidence is and how to fix it. It runs offline: no network calls and no model \
behind it.\n\nExit codes: 0 = culprit named, 2 = crashdoctor doesn't know, 1 = bad usage or \
unreadable file.";

#[derive(Debug, Parser)]
#[command(name = "crashdoctor", version, about = ABOUT, long_about = LONG_ABOUT)]
#[rustfmt::skip]
struct Cli {
    #[arg(value_name = "CRASH_REPORT", help = "Crash report to analyze (crash-*.txt)", required_unless_present = "ui")]
    crash_file: Option<PathBuf>,

    #[arg(long, help = "Serve a local web UI in the browser instead of printing to stdout")]
    ui:         bool,

    #[arg(long, value_name = "PORT", default_value_t = 8787, help = "Port for the web UI (0 picks a free one)")]
    port:       u16,

    #[arg(long, value_name = "DIR", help = "Extra folder to look for crash reports in the web UI")]
    dir:        Option<PathBuf>,

    #[arg(long, help = "Do not open a browser when the web UI starts")]
    no_open:    bool,

    #[arg(long, help = "Dump what the parser read and which rules were evaluated")]
    debug:     bool,
}

struct Out {
    text: String,
}

impl Out {
    fn new() -> Out {
        Out {
            text: String::new(),
        }
    }

    fn line(&mut self, text: &str) {
        self.text.push_str(text);
        self.text.push('\n');
    }

    fn blank(&mut self) {
        self.text.push('\n');
    }

    fn field(&mut self, label: &str, value: &str) {
        self.text.push_str(label);
        self.text.push_str(": ");
        self.text.push_str(value);
        self.text.push('\n');
    }

    fn block(&mut self, label: &str, lines: &[String]) {
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                self.field(label, line);
            } else {
                self.line(line);
            }
        }
    }

    fn flush(&self) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(self.text.as_bytes());
        let _ = stdout.flush();
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return clap_exit(err),
    };
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn clap_exit(err: clap::Error) -> ExitCode {
    let asking = err.kind() == ErrorKind::DisplayHelp || err.kind() == ErrorKind::DisplayVersion;
    let _ = err.print();
    if asking {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    if cli.ui {
        let options = ui::UiOptions {
            port: cli.port,
            extra_dir: cli.dir.clone(),
            open_browser: !cli.no_open,
        };
        ui::serve(options).context("could not start the web UI")?;
        return Ok(ExitCode::SUCCESS);
    }

    let Some(path) = &cli.crash_file else {
        anyhow::bail!("pass a crash report or use --ui");
    };
    let report = report::parse_file(path)?;
    let ev = rules::evaluate(&report);

    let mut out = Out::new();
    print_header(&mut out, &report);
    print_diagnosis(&mut out, &ev.diagnosis);
    if cli.debug {
        print_debug(&mut out, &report, &ev);
    }
    out.flush();

    if ev.diagnosis.culprit.is_some() {
        return Ok(ExitCode::SUCCESS);
    }
    Ok(ExitCode::from(2))
}

fn print_header(out: &mut Out, r: &Report) {
    out.line(&format!("crashdoctor · {}", r.source.display()));

    let mut bits: Vec<String> = vec![loader_line(r)];
    if r.mc_version.is_some() {
        bits.push("Minecraft ".to_string() + r.mc_version.as_deref().unwrap_or(""));
    }
    if r.mod_count() > 0 {
        bits.push(r.mod_count().to_string() + " mods");
    }
    if let Some(java) = &r.java_version {
        bits.push("Java ".to_string() + java);
    }
    out.line(&bits.join(" · "));

    if r.loader == Loader::Fabric || r.loader == Loader::NeoForge {
        out.line(&format!(
            "note: this report comes from {}. crashdoctor is tuned for Forge 1.20.1, so the reading may be incomplete",
            r.loader.label()
        ));
    }
}

fn loader_line(r: &Report) -> String {
    if r.loader == Loader::Unknown {
        return "unknown loader".to_string();
    }
    let tmp = r.loader.label().to_string();
    if r.loader_version.is_none() {
        return tmp;
    }
    tmp + " " + r.loader_version.as_deref().unwrap_or("")
}

fn print_diagnosis(out: &mut Out, d: &Diagnosis) {
    out.blank();
    out.field("CULPRIT", d.culprit.as_deref().unwrap_or("unknown"));
    out.field("CAUSE", &d.cause);
    out.block("EVIDENCE", &d.evidence);
    out.block("FIX", &d.fix);
    out.block("DEV TIP", &d.dev_tip);
    out.field(
        "CONFIDENCE",
        &(d.confidence.label().to_string() + " " + d.confidence.face()),
    );
}

// TODO: dump the parsed sections here too, it is the first thing I want when a rule misses
fn print_debug(out: &mut Out, r: &Report, ev: &Evaluation) {
    out.blank();
    out.line("-- debug --");
    out.field("bytes read", &r.raw.len().to_string());
    out.field("description", r.description.as_deref().unwrap_or("-"));

    let sections = if r.sections.is_empty() {
        "none".to_string()
    } else {
        let names: Vec<&str> = r.sections.iter().map(|s| s.name.as_str()).collect();
        names.join(", ")
    };
    out.field("sections", &sections);
    out.field("frames", &r.frames.len().to_string());

    let exceptions = if r.exceptions.is_empty() {
        "none".to_string()
    } else {
        let all: Vec<String> = r.exceptions.iter().map(|e| e.text()).collect();
        all.join(" | ")
    };
    out.field("exceptions", &exceptions);

    let labels: Vec<String> = r.mods.iter().map(|m| m.label()).collect();
    let mods = r.mods.len().to_string() + " (" + &labels.join(", ") + ")";
    out.field("mods", &mods);

    let flags = if r.jvm_flags.is_empty() {
        "none".to_string()
    } else {
        r.jvm_flags.join(" ")
    };
    out.field("jvm flags", &flags);

    let mut xmx = "-".to_string();
    if let Some(v) = r.xmx_mb {
        xmx = v.to_string() + " MB";
    }
    let mut heap = "-".to_string();
    if let Some(v) = r.heap_max_mb {
        heap = v.to_string() + " MiB";
    }
    out.line(&format!("-Xmx: {xmx} · heap max: {heap}"));

    for (name, hit) in &ev.checks {
        let verdict = if *hit { "matched" } else { "no" };
        out.line(&("rule ".to_owned() + name + ": " + verdict));
    }
    for w in r.warnings.iter() {
        out.line(&format!("warning: {w}"));
    }
    if ev.diagnosis.confidence != Confidence::Low {
        return;
    }
    out.line("note: nothing matched with high confidence");
}
