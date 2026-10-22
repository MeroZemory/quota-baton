//! Thread-local UI copy. Data-derived names must never be passed through translation.
pub use qb_core::config::UiLanguage;
use std::{borrow::Cow, cell::Cell};
pub mod forecast;
mod strings;

thread_local! { static LANGUAGE: Cell<UiLanguage> = const { Cell::new(UiLanguage::Ko) }; }

pub fn language() -> UiLanguage {
    LANGUAGE.with(Cell::get)
}
pub fn set_language(value: UiLanguage) {
    LANGUAGE.with(|slot| slot.set(value));
}

/// Restores the caller's language even when the closure unwinds.
pub fn with_language<T>(value: UiLanguage, f: impl FnOnce() -> T) -> T {
    struct Restore(UiLanguage);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_language(self.0);
        }
    }
    let restore = Restore(language());
    set_language(value);
    let result = f();
    drop(restore);
    result
}

pub fn text(ko: &str) -> Cow<'_, str> {
    if language() == UiLanguage::En
        && let Some(en) = strings::STRINGS
            .iter()
            .chain(crate::i18n_shell_strings::STRINGS)
            .chain(crate::i18n_client_strings::STRINGS)
            .find_map(|(key, en)| (*key == ko).then_some(*en))
    {
        return Cow::Borrowed(en);
    }
    Cow::Borrowed(ko)
}

/// Borrowed-label companion; unknown static copy retains its original value.
pub fn static_text(ko: &'static str) -> &'static str {
    match text(ko) {
        Cow::Borrowed(value) => value,
        Cow::Owned(_) => unreachable!("translation table is static"),
    }
}

/// Formats only `{}` / `{N}` fields. `{{` and `}}` escape braces.
/// Missing arguments and malformed fields are retained literally. Values are appended
/// once and are never reparsed, translated, or interpreted as format instructions.
pub fn format(ko_template: &str, args: &[String]) -> String {
    let template = text(ko_template);
    let mut out = String::with_capacity(template.len());
    let mut chars = template.char_indices().peekable();
    let mut implicit = 0usize;
    while let Some((start, ch)) = chars.next() {
        if ch == '{' {
            if chars.peek().is_some_and(|(_, c)| *c == '{') {
                chars.next();
                out.push('{');
                continue;
            }
            let tail = &template[start + 1..];
            if let Some(end) = tail.find('}') {
                let field = &tail[..end];
                let index = if field.is_empty() {
                    let i = implicit;
                    implicit = implicit.saturating_add(1);
                    Some(i)
                } else if field.bytes().all(|b| b.is_ascii_digit()) {
                    field.parse::<usize>().ok()
                } else {
                    None
                };
                if let Some(value) = index.and_then(|i| args.get(i)) {
                    out.push_str(value);
                    let stop = start + 1 + end;
                    while chars.peek().is_some_and(|(i, _)| *i <= stop) {
                        chars.next();
                    }
                    continue;
                }
            }
        } else if ch == '}' && chars.peek().is_some_and(|(_, c)| *c == '}') {
            chars.next();
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn korean_default_and_unknown_names_are_unchanged() {
        assert_eq!(language(), UiLanguage::Ko);
        assert_eq!(text("알 수 없는 사용자 폴더"), "알 수 없는 사용자 폴더");
        with_language(UiLanguage::En, || {
            assert_eq!(text("알 수 없는 사용자 폴더"), "알 수 없는 사용자 폴더")
        });
    }
    #[test]
    fn nested_scopes_and_unwind_restore_language() {
        with_language(UiLanguage::En, || {
            assert_eq!(language(), UiLanguage::En);
            with_language(UiLanguage::Ko, || assert_eq!(language(), UiLanguage::Ko));
            let _ =
                std::panic::catch_unwind(|| with_language(UiLanguage::Ko, || panic!("fixture")));
            assert_eq!(language(), UiLanguage::En);
            assert_eq!(std::thread::spawn(language).join().unwrap(), UiLanguage::Ko);
        });
        assert_eq!(language(), UiLanguage::Ko);
    }
    #[test]
    fn placeholders_do_not_reparse_inserted_values() {
        assert_eq!(
            format(
                "{1} / {0} / {} / {{x}}",
                &["{1} 사용자".into(), "two".into()]
            ),
            "two / {1} 사용자 / {1} 사용자 / {x}"
        );
        assert_eq!(
            format("{99} {oops} {999999999999999999999999999999999999} {", &[]),
            "{99} {oops} {999999999999999999999999999999999999} {"
        );
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    #[test]
    fn catalog_is_unique_and_korean_copy_is_exact() {
        let mut seen = std::collections::BTreeSet::new();
        for (ko, en) in strings::STRINGS
            .iter()
            .chain(crate::i18n_client_strings::STRINGS)
            .chain(crate::i18n_shell_strings::STRINGS)
        {
            assert!(seen.insert(ko), "duplicate UI source key: {ko}");
            with_language(UiLanguage::Ko, || assert_eq!(text(ko), *ko));
            with_language(UiLanguage::En, || assert_eq!(text(ko), *en));
        }
    }
    #[test]
    fn source_templates_translate_before_inserting_values() {
        with_language(UiLanguage::En, || {
            assert_eq!(
                format("세션 {}", &["사용 중 {0}".into()]),
                "Session 사용 중 {0}"
            );
            assert_eq!(static_text("사용량"), "Usage");
        });
        assert_eq!(
            format("세션 {}", &["사용 중 {0}".into()]),
            "세션 사용 중 {0}"
        );
    }
}

/// Locale-specific presentation dates. The caller retains its existing timezone.
pub fn date_time<T: chrono::TimeZone>(at: &chrono::DateTime<T>, ko_pattern: &str) -> String
where
    T::Offset: std::fmt::Display,
{
    at.format(if language() == UiLanguage::En {
        "%b %-d %H:%M"
    } else {
        ko_pattern
    })
    .to_string()
}

/// Compact elapsed time for English UI; Korean call sites retain their old formatter.
pub fn duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        if seconds > 0 {
            format!("{hours}h {minutes}m {seconds}s")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else if minutes > 0 {
        if seconds > 0 {
            format!("{minutes}m {seconds}s")
        } else {
            format!("{minutes}m")
        }
    } else {
        format!("{seconds}s")
    }
}
