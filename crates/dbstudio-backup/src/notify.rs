//! Texts of OS notifications (and CLI summaries) in Vietnamese and English.
//!
//! The language is `AppSettings.language`; `System` follows the OS locale (`vi*` is Vietnamese,
//! anything else English). The table lives here, not in the frontend i18n files, so that
//! notifications work while the window is closed and the CLI can print the same texts.

use dbstudio_store::UiLanguage;

/// Language of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Vietnamese.
    Vi,
    /// English.
    En,
}

/// Language for the setting `language` given the OS `locale` (e.g. `vi-VN`, `en_US`).
pub fn resolve_language(language: UiLanguage, locale: Option<&str>) -> Lang {
    match language {
        UiLanguage::Vi => Lang::Vi,
        UiLanguage::En => Lang::En,
        UiLanguage::System => match locale {
            Some(l) if l.trim().to_ascii_lowercase().starts_with("vi") => Lang::Vi,
            _ => Lang::En,
        },
    }
}

/// [`resolve_language`] with the real OS locale.
pub fn language_for(language: UiLanguage) -> Lang {
    match language {
        UiLanguage::System => resolve_language(language, sys_locale::get_locale().as_deref()),
        other => resolve_language(other, None),
    }
}

/// Longest error text put in a notification (characters).
const MAX_ERROR_CHARS: usize = 160;

/// One line, cut to [`MAX_ERROR_CHARS`].
pub fn shorten_error(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_ERROR_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_ERROR_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// `3.0 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn duration_text(lang: Lang, ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let (hu, mu, su, ms_u) = match lang {
        Lang::Vi => ("giờ", "phút", "giây", "ms"),
        Lang::En => ("h", "min", "s", "ms"),
    };
    match (h, m) {
        (0, 0) if secs == 0 => format!("{ms} {ms_u}"),
        (0, 0) => format!("{s} {su}"),
        (0, _) => format!("{m} {mu} {s} {su}"),
        _ => format!("{h} {hu} {m} {mu}"),
    }
}

/// What the finished run produced, for the success text.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunFacts {
    /// Size of the file.
    pub size_bytes: Option<u64>,
    /// Duration of the run.
    pub duration_ms: Option<u64>,
}

/// `(title, body)` of the notification for a successful backup.
pub fn backup_succeeded(lang: Lang, job: &str, facts: RunFacts) -> (String, String) {
    let mut details = Vec::new();
    if let Some(b) = facts.size_bytes {
        details.push(human_size(b));
    }
    if let Some(ms) = facts.duration_ms {
        details.push(duration_text(lang, ms));
    }
    let tail = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    match lang {
        Lang::Vi => (
            "Sao lưu DBStudio".into(),
            format!("{job}: đã hoàn tất{tail}"),
        ),
        Lang::En => ("DBStudio backup".into(), format!("{job}: finished{tail}")),
    }
}

/// `(title, body)` of the notification for a failed backup (`error` is shortened).
pub fn backup_failed(lang: Lang, job: &str, error: Option<&str>) -> (String, String) {
    let unknown = match lang {
        Lang::Vi => "lỗi không rõ",
        Lang::En => "unknown error",
    };
    let msg = error
        .map(shorten_error)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| unknown.to_string());
    match lang {
        Lang::Vi => ("Sao lưu DBStudio thất bại".into(), format!("{job}: {msg}")),
        Lang::En => ("DBStudio backup failed".into(), format!("{job}: {msg}")),
    }
}

/// `(title, body)` of the warning shown when quitting while jobs run.
pub fn quit_busy(lang: Lang, jobs: usize, confirm_secs: i64) -> (String, String) {
    match lang {
        Lang::Vi => (
            "DBStudio đang bận".into(),
            format!(
                "{jobs} tác vụ nền đang chạy. Chọn Thoát lần nữa trong {confirm_secs} giây để dừng chúng và thoát."
            ),
        ),
        Lang::En => (
            "DBStudio is busy".into(),
            format!(
                "{jobs} background job(s) are running. Choose Quit again within {confirm_secs} s to stop them and exit."
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_follows_the_setting_then_the_os_locale() {
        assert_eq!(resolve_language(UiLanguage::Vi, Some("en-US")), Lang::Vi);
        assert_eq!(resolve_language(UiLanguage::En, Some("vi-VN")), Lang::En);
        assert_eq!(
            resolve_language(UiLanguage::System, Some("vi-VN")),
            Lang::Vi
        );
        assert_eq!(resolve_language(UiLanguage::System, Some("vi")), Lang::Vi);
        assert_eq!(
            resolve_language(UiLanguage::System, Some("fr-FR")),
            Lang::En
        );
        assert_eq!(resolve_language(UiLanguage::System, None), Lang::En);
    }

    #[test]
    fn success_and_failure_texts_in_both_languages() {
        let facts = RunFacts {
            size_bytes: Some(3 * 1024 * 1024),
            duration_ms: Some(75_000),
        };
        let (t, b) = backup_succeeded(Lang::Vi, "hằng đêm", facts);
        assert_eq!(t, "Sao lưu DBStudio");
        assert_eq!(b, "hằng đêm: đã hoàn tất (3.0 MB, 1 phút 15 giây)");
        let (t, b) = backup_succeeded(Lang::En, "nightly", facts);
        assert_eq!(t, "DBStudio backup");
        assert_eq!(b, "nightly: finished (3.0 MB, 1 min 15 s)");
        let (_, b) = backup_succeeded(Lang::En, "n", RunFacts::default());
        assert_eq!(b, "n: finished");
        let (t, b) = backup_failed(Lang::Vi, "j", Some("disk\nfull"));
        assert_eq!(t, "Sao lưu DBStudio thất bại");
        assert_eq!(b, "j: disk full");
        assert_eq!(backup_failed(Lang::Vi, "j", None).1, "j: lỗi không rõ");
        assert_eq!(backup_failed(Lang::En, "j", None).1, "j: unknown error");
    }

    #[test]
    fn long_errors_are_cut_to_one_line() {
        let long = "x".repeat(500);
        let s = shorten_error(&format!("a\n{long}"));
        assert_eq!(s.chars().count(), MAX_ERROR_CHARS);
        assert!(s.ends_with('…') && !s.contains('\n'));
    }

    #[test]
    fn durations_use_the_language_units() {
        assert_eq!(duration_text(Lang::En, 800), "800 ms");
        assert_eq!(duration_text(Lang::Vi, 5_000), "5 giây");
        assert_eq!(duration_text(Lang::En, 3_660_000), "1 h 1 min");
    }
}
