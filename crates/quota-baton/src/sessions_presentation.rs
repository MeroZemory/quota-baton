//! Pure, metadata-only presentation for a conversation import preview.
//!
//! The caller owns metadata collection, language selection, confirmation, and IO.

use std::fmt::Write;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Language {
    Ko,
    En,
}

/// All sizes describe the supplied metadata (including compressed file sizes).
/// No path is opened and no conversation content is decoded by this module.
pub(crate) struct Preview<'a> {
    pub(crate) id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) bytes: u64,
    pub(crate) source_home: &'a str,
    pub(crate) target_home: &'a str,
    pub(crate) profile_home: &'a str,
    /// Raw goal states are localized; unknown values remain readable verbatim.
    pub(crate) goal_before: Option<&'a str>,
    pub(crate) goal_after: Option<&'a str>,
    pub(crate) verbose: Option<Verbose<'a>>,
}

pub(crate) struct Verbose<'a> {
    pub(crate) files: &'a [FileLink<'a>],
    pub(crate) ancestors: &'a [&'a str],
}

pub(crate) struct FileLink<'a> {
    pub(crate) source: &'a str,
    pub(crate) target: &'a str,
}

/// Plain output, suitable for both terminals and redirected output. The final
/// prompt deliberately has no newline so a caller can read a reply beside it.
#[cfg(test)]
pub(crate) fn format_preview(language: Language, preview: &Preview<'_>) -> String {
    let mut out = format_body(language, preview);
    out.push_str(match language {
        Language::Ko => "move 또는 share를 입력하세요 (취소: Enter) > ",
        Language::En => "Enter move or share (cancel: Enter) > ",
    });
    out
}

/// Preview content only, ending in a blank line. Confirmation owns its prompt.
pub(crate) fn format_body(language: Language, preview: &Preview<'_>) -> String {
    let id = short_id(preview.id);
    let title = plain(preview.title);
    let size = size(preview.bytes);
    let source = readable_path(preview.source_home, preview.profile_home);
    let target = readable_path(preview.target_home, preview.profile_home);
    let goal = goal_text(language, preview.goal_before, preview.goal_after);
    let mut out = String::new();
    match language {
        Language::Ko => {
            let _ = writeln!(out, "대화 {id}를 qb로 가져옵니다  ({title} · 기록 {size})\n");
            let _ = writeln!(out, "  어디서   {source}    기본 Codex 홈");
            let _ = writeln!(out, "  어디로   {target}    codex-qb 홈");
            let _ = writeln!(out, "  목표     {goal}\n");
            out.push_str("  move    원본 홈에서 빼고 qb로 옮깁니다. 이후 원본 홈에서는 열 수 없습니다.  (추천)\n");
            out.push_str("  share   원본 홈에도 남겨 둡니다. 두 곳에서 동시에 열지 마세요.\n");
        }
        Language::En => {
            let _ = writeln!(out, "Bring conversation {id} into qb  ({title} · history {size})\n");
            let _ = writeln!(out, "  From    {source}    default Codex home");
            let _ = writeln!(out, "  To      {target}    codex-qb home");
            let _ = writeln!(out, "  Goal    {goal}\n");
            out.push_str("  move    Move into qb and remove from the source home. It can no longer be opened there.  (recommended)\n");
            out.push_str("  share   Keep it in the source home too. Do not open it in both places at once.\n");
        }
    }
    if let Some(verbose) = &preview.verbose {
        if !verbose.files.is_empty() {
            out.push_str(match language {
                Language::Ko => "\n  연결할 파일:\n",
                Language::En => "\n  Files to link:\n",
            });
            for file in verbose.files {
                let source = readable_path(file.source, preview.profile_home);
                let target = readable_path(file.target, preview.profile_home);
                let _ = writeln!(out, "    {source} → {target}");
            }
        }
        if !verbose.ancestors.is_empty() {
            out.push_str(match language {
                Language::Ko => "\n  상위 대화:\n",
                Language::En => "\n  Ancestor conversations:\n",
            });
            for ancestor in verbose.ancestors {
                let _ = writeln!(out, "    {}", short_id(ancestor));
            }
        }
    }
    let id = plain(preview.id);
    match language {
        Language::Ko => {
            let _ = writeln!(out, "\n  나중에 되돌리기: quota-baton sessions return {id}\n");
        }
        Language::En => {
            let _ = writeln!(out, "\n  Return later: quota-baton sessions return {id}\n");
        }
    }
    out
}

fn goal_text(language: Language, before: Option<&str>, after: Option<&str>) -> String {
    let absent = match language {
        Language::Ko => "없음",
        Language::En => "none",
    };
    let after_text = goal_state(language, after.unwrap_or(absent));
    if before == after {
        let unchanged = match language {
            Language::Ko => "그대로 유지",
            Language::En => "unchanged",
        };
        format!("{after_text} ({unchanged})")
    } else {
        format!("{} → {after_text}", goal_state(language, before.unwrap_or(absent)))
    }
}

fn goal_state(language: Language, state: &str) -> String {
    plain(match (language, state) {
        (Language::Ko, "active") => "진행 중",
        (Language::Ko, "usage_limited") => "사용량 한도 대기",
        (Language::Ko, "paused") => "일시정지",
        (Language::Ko, "blocked") => "외부 조건 대기",
        (Language::Ko, "budget_limited") => "예산 소진",
        (Language::Ko, "complete") => "완료",
        (Language::En, "usage_limited") => "waiting for usage limit reset",
        (Language::En, "blocked") => "waiting for external conditions",
        (Language::En, "budget_limited") => "budget exhausted",
        _ => state,
    })
}

/// Binary KB/MB/GB with at most one decimal, including compressed byte counts.
pub(crate) fn size(bytes: u64) -> String {
    let (divisor, unit) = if bytes >= 1024 * 1024 * 1024 {
        (1024_u64.pow(3), "GB")
    } else if bytes >= 1024 * 1024 {
        (1024_u64.pow(2), "MB")
    } else {
        (1024, "KB")
    };
    let number = format!("{:.1}", bytes as f64 / divisor as f64);
    format!("{} {unit}", number.strip_suffix(".0").unwrap_or(&number))
}

fn short_id(id: &str) -> String {
    plain(id).chars().take(8).collect()
}

// Strip incoming control characters as well as avoiding terminal escapes of our
// own: even a title originating in metadata must not control terminal output.
fn plain(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

fn strip_extended_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    }
}

pub(crate) fn readable_path(path: &str, profile: &str) -> String {
    let path = strip_extended_prefix(path);
    let profile = strip_extended_prefix(profile);
    let profile = profile.trim_end_matches(['\\', '/']);
    if !profile.is_empty()
        && path
            .get(..profile.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(profile))
    {
        let rest = &path[profile.len()..];
        if rest.is_empty() || rest.starts_with(['\\', '/']) {
            return format!("~{}", plain(rest));
        }
    }
    plain(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview() -> Preview<'static> {
        Preview {
            id: "01234567-89ab-cdef-0123-456789abcdef",
            title: "sample-conversation",
            bytes: 105_888_757,
            source_home: r"\\?\C:\Users\Sample\.codex",
            target_home: r"\\?\C:\Users\Sample\.codex-acct\qb",
            profile_home: r"C:\Users\Sample",
            goal_before: Some("blocked"),
            goal_after: Some("blocked"),
            verbose: None,
        }
    }

    const KO: &str = r#"대화 01234567를 qb로 가져옵니다  (sample-conversation · 기록 101 MB)

  어디서   ~\.codex    기본 Codex 홈
  어디로   ~\.codex-acct\qb    codex-qb 홈
  목표     외부 조건 대기 (그대로 유지)

  move    원본 홈에서 빼고 qb로 옮깁니다. 이후 원본 홈에서는 열 수 없습니다.  (추천)
  share   원본 홈에도 남겨 둡니다. 두 곳에서 동시에 열지 마세요.

  나중에 되돌리기: quota-baton sessions return 01234567-89ab-cdef-0123-456789abcdef

move 또는 share를 입력하세요 (취소: Enter) > "#;

    const EN: &str = r#"Bring conversation 01234567 into qb  (sample-conversation · history 101 MB)

  From    ~\.codex    default Codex home
  To      ~\.codex-acct\qb    codex-qb home
  Goal    waiting → active

  move    Move into qb and remove from the source home. It can no longer be opened there.  (recommended)
  share   Keep it in the source home too. Do not open it in both places at once.

  Return later: quota-baton sessions return 01234567-89ab-cdef-0123-456789abcdef

Enter move or share (cancel: Enter) > "#;

    #[test]
    fn golden_ko_plain_metadata() {
        assert_eq!(format_preview(Language::Ko, &preview()), KO);
    }

    #[test]
    fn golden_en_zst_metadata() {
        // A compressed history supplies its on-disk byte count, never decoded
        // contents or a rollout filename to the ordinary preview.
        let mut input = preview();
        input.goal_before = Some("waiting");
        input.goal_after = Some("active");
        assert_eq!(format_preview(Language::En, &input), EN);
    }

    #[test]
    fn body_excludes_only_the_final_prompt() {
        for (language, golden, prompt) in [
            (Language::Ko, KO, "move 또는 share를 입력하세요 (취소: Enter) > "),
            (Language::En, EN, "Enter move or share (cancel: Enter) > "),
        ] {
            let mut input = preview();
            if language == Language::En {
                input.goal_before = Some("waiting");
                input.goal_after = Some("active");
            }
            let body = format_body(language, &input);
            assert_eq!(body, golden.strip_suffix(prompt).unwrap());
            assert!(body.ends_with("\n\n"));
            assert_eq!(format!("{body}{prompt}"), format_preview(language, &input));
        }
    }

    #[test]
    fn raw_goal_states_are_localized() {
        for (raw, ko, en) in [
            ("active", "진행 중", "active"),
            ("usage_limited", "사용량 한도 대기", "waiting for usage limit reset"),
            ("paused", "일시정지", "paused"),
            ("blocked", "외부 조건 대기", "waiting for external conditions"),
            ("budget_limited", "예산 소진", "budget exhausted"),
            ("complete", "완료", "complete"),
            ("future_state", "future_state", "future_state"),
        ] {
            assert_eq!(goal_state(Language::Ko, raw), ko);
            assert_eq!(goal_state(Language::En, raw), en);
        }
        assert_eq!(goal_text(Language::Ko, Some("active"), Some("paused")), "진행 중 → 일시정지");
        assert_eq!(goal_text(Language::En, Some("blocked"), Some("blocked")), "waiting for external conditions (unchanged)");
    }

    #[test]
    fn golden_verbose_files_and_ancestors() {
        let files = [FileLink {
            source: r"\\?\C:\Users\Sample\.codex\sessions\rollout-sample.jsonl.zst",
            target: r"\\?\C:\Users\Sample\.codex-acct\qb\sessions\rollout-sample.jsonl.zst",
        }, FileLink {
            source: r"C:\Users\Sample\.codex\sessions\rollout-parent.jsonl",
            target: r"C:\Users\Sample\.codex-acct\qb\sessions\rollout-parent.jsonl",
        }];
        let ancestors = ["abcdef01-2345-6789-abcd-ef0123456789"];
        let mut input = preview();
        input.verbose = Some(Verbose { files: &files, ancestors: &ancestors });
        let expected = KO.replace(
            "\n  나중에 되돌리기:",
            r#"
  연결할 파일:
    ~\.codex\sessions\rollout-sample.jsonl.zst → ~\.codex-acct\qb\sessions\rollout-sample.jsonl.zst
    ~\.codex\sessions\rollout-parent.jsonl → ~\.codex-acct\qb\sessions\rollout-parent.jsonl

  상위 대화:
    abcdef01

  나중에 되돌리기:"#,
        );
        assert_eq!(format_preview(Language::Ko, &input), expected);
    }

    #[test]
    fn golden_non_tty_has_only_newline_controls() {
        let mut input = preview();
        input.title = "sample-\u{1b}conversation\r\n\t";
        let output = format_preview(Language::Ko, &input);
        assert_eq!(output, KO);
        assert!(output.chars().all(|c| !c.is_control() || c == '\n'));
    }

    #[test]
    fn binary_sizes_have_at_most_one_decimal() {
        for (bytes, expected) in [
            (0, "0 KB"),
            (512, "0.5 KB"),
            (1024, "1 KB"),
            (1536, "1.5 KB"),
            (1024 * 1024, "1 MB"),
            (1024 * 1024 * 1024, "1 GB"),
            (1536 * 1024 * 1024, "1.5 GB"),
        ] {
            assert_eq!(size(bytes), expected);
        }
    }

    #[test]
    fn profile_replacement_requires_a_path_boundary() {
        assert_eq!(readable_path(r"C:\Users\Sample2\.codex", r"C:\Users\Sample"), r"C:\Users\Sample2\.codex");
        assert_eq!(readable_path(r"\\?\C:\USERS\SAMPLE\.codex", r"C:\Users\Sample\"), r"~\.codex");
        assert_eq!(readable_path(r"C:\Users\Sample", r"C:\Users\Sample"), "~");
        assert_eq!(readable_path(r"\\?\UNC\server\share\home", r"\\server\share"), r"~\home");
        assert_eq!(readable_path("/home/sample/.codex", "/home/sample"), "~/.codex");
        assert_eq!(readable_path("/home/예시/.codex", "/home/sample"), "/home/예시/.codex");
        assert_eq!(readable_path("/home/sample", ""), "/home/sample");
    }

    #[test]
    fn missing_and_unchanged_goals_and_unicode_ids() {
        assert_eq!(goal_text(Language::En, None, None), "none (unchanged)");
        assert_eq!(goal_text(Language::Ko, None, Some("진행 중")), "없음 → 진행 중");
        assert_eq!(goal_text(Language::En, Some("active"), Some("active")), "active (unchanged)");
        assert_eq!(short_id("가나다라마바사아자"), "가나다라마바사아");
    }
}
