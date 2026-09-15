macro_rules! lazy_re {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static regex::Regex {
            static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            RE.get_or_init(|| regex::Regex::new($pattern).expect("bad regex"))
        }
    };
}

pub mod model;
pub mod report;
pub mod rules;
pub mod ui;
