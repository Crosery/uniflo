//! Message language of the command line: Chinese when the system language is Chinese, English
//! otherwise. What people read is translated; machine output (JSON, TSV, MCP tool results,
//! `uniflo context`) is not.
//!
//! The first source that is set decides: `UNIFLO_LANG`, `LC_ALL`, `LC_MESSAGES`, the desktop
//! language (macOS `AppleLanguages`, the Windows display language), then `LANG`. The desktop
//! language goes before `LANG` because terminals set `LANG=en_US.UTF-8` whatever the system
//! language is.

/// `tr!("中文 {x}", "English {x}", args…)`: the message formatted in the current language.
macro_rules! tr {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::zh() { format!($zh $(, $arg)*) } else { format!($en $(, $arg)*) }
    };
}
pub(crate) use tr;

#[cfg(not(test))]
pub fn zh() -> bool {
    static ZH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ZH.get_or_init(|| is_zh(&|k| std::env::var(k).ok(), &desktop_language))
}

// Unit tests read Chinese unless a test switches its own thread to English.
#[cfg(test)]
thread_local!(static ZH: std::cell::Cell<bool> = const { std::cell::Cell::new(true) });

#[cfg(test)]
pub fn zh() -> bool {
    ZH.with(|z| z.get())
}

#[cfg(test)]
pub fn set_zh(on: bool) {
    ZH.with(|z| z.set(on));
}

fn is_zh(var: &dyn Fn(&str) -> Option<String>, desktop: &dyn Fn() -> Option<String>) -> bool {
    let set = |k: &str| var(k).filter(|v| !v.trim().is_empty());
    set("UNIFLO_LANG")
        .or_else(|| set("LC_ALL"))
        .or_else(|| set("LC_MESSAGES"))
        .or_else(desktop)
        .or_else(|| set("LANG"))
        .is_some_and(|tag| tag.trim().to_ascii_lowercase().starts_with("zh"))
}

/// First entry of the user's preferred languages, e.g. `zh-Hans-CN`.
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
fn desktop_language() -> Option<String> {
    let out = uniflo_core::util::silent_cmd("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().map(|l| l.trim().trim_matches([',', '"', ' '])).find(|l| !l.is_empty() && *l != "(")?;
    (first != ")").then(|| first.to_owned())
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
fn desktop_language() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // SAFETY: no arguments, no preconditions; returns a LANGID.
    let id = unsafe { GetUserDefaultUILanguage() };
    // The low 10 bits are the primary language; LANG_CHINESE = 0x04.
    Some(if id & 0x3ff == 0x04 { "zh" } else { "en" }.to_owned())
}

#[cfg(not(any(target_os = "macos", windows)))]
#[cfg_attr(test, allow(dead_code))]
fn desktop_language() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn first_set_source_decides() {
        let none = || None;
        let zh_desktop = || Some("zh-Hans-CN".to_owned());
        let en_desktop = || Some("en-CN".to_owned());
        assert!(!is_zh(&env(&[]), &none), "nothing set: English");
        assert!(is_zh(&env(&[("LANG", "zh_CN.UTF-8")]), &none));
        assert!(!is_zh(&env(&[("LANG", "en_US.UTF-8")]), &none));
        // a terminal's LANG=en_US does not hide a Chinese desktop
        assert!(is_zh(&env(&[("LANG", "en_US.UTF-8")]), &zh_desktop));
        assert!(!is_zh(&env(&[("LANG", "zh_CN.UTF-8")]), &en_desktop));
        // explicit locale variables and UNIFLO_LANG win over the desktop
        assert!(!is_zh(&env(&[("LC_ALL", "C")]), &zh_desktop));
        assert!(is_zh(&env(&[("LC_MESSAGES", "zh_TW.UTF-8")]), &en_desktop));
        assert!(!is_zh(&env(&[("UNIFLO_LANG", "en"), ("LC_ALL", "zh_CN.UTF-8")]), &zh_desktop));
        assert!(is_zh(&env(&[("UNIFLO_LANG", "zh"), ("LC_ALL", "en_US.UTF-8")]), &en_desktop));
        // empty values count as unset
        assert!(is_zh(&env(&[("UNIFLO_LANG", " "), ("LC_ALL", "")]), &zh_desktop));
    }

    #[test]
    fn tr_formats_the_chosen_language() {
        let n = 3;
        assert_eq!(tr!("{n} 个", "{n} items"), "3 个");
        set_zh(false);
        assert_eq!(tr!("{} 个", "{} items", n + 1), "4 items");
        set_zh(true);
    }
}
