//! One typed confirmation after the caller prints the complete session plan.

use anyhow::{Result, anyhow, bail};
use std::io::{self, IsTerminal, Write};

const CANCELLED: &str = "취소했습니다. 아무것도 바뀌지 않았습니다.";
const NO_TERMINAL: &str = "표준 입력과 출력이 모두 터미널이어야 합니다. 수동 명령을 실행해 주세요.";
const OUTPUT_FAILED: &str = "확인 안내를 출력하지 못했습니다.";
const INPUT_FAILED: &str = "확인 입력을 읽지 못해 취소했습니다.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Move,
    Share,
    Resolve,
}

pub(crate) fn ask_resolve(manual: &str) -> Result<()> {
    ask(false, Mode::Resolve, false, manual).map(drop)
}

/// Ask once, with one retry for unknown input; Enter and I/O failure cancel.
/// `yes` selects the caller's default without inspecting terminals or reading.
pub(crate) fn ask(yes: bool, default: Mode, allow_share: bool, manual: &str) -> Result<Mode> {
    if yes {
        return Ok(default);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let terminals = (stdin.is_terminal(), stdout.is_terminal());
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Confirmation, None)?;
    ask_with_io(
        false,
        default,
        allow_share,
        manual,
        terminals,
        crate::sessions_progress::read_line,
        &mut stdout.lock(),
    )
}

fn ask_with_io(
    yes: bool,
    default: Mode,
    allow_share: bool,
    manual: &str,
    terminals: (bool, bool),
    mut read_line: impl FnMut(&mut String) -> io::Result<usize>,
    output: &mut impl Write,
) -> Result<Mode> {
    if yes {
        return Ok(default);
    }
    if !terminals.0 || !terminals.1 {
        writeln!(
            output,
            "터미널에서 직접 확인해야 합니다. 수동으로 실행할 명령:\n{manual}"
        )
        .and_then(|()| output.flush())
        .map_err(|_| anyhow!(OUTPUT_FAILED))?;
        bail!(NO_TERMINAL);
    }

    let prompt = if default == Mode::Resolve {
        "이 대화의 소유 위치를 복구하려면 'resolve'를 정확히 입력하세요: "
    } else if allow_share && crate::sessions_progress::english() {
        "Enter move or share (cancel: Enter) > "
    } else if allow_share {
        "move 또는 share를 입력하세요 (취소: Enter) > "
    } else if crate::sessions_progress::english() {
        "Move out of the source home. Enter move to continue (cancel: Enter) > "
    } else {
        "원본 홈에서 빼고 옮깁니다. 계속하려면 move를 입력하세요 (취소: Enter) > "
    };
    for attempt in 0..2 {
        output
            .write_all(prompt.as_bytes())
            .and_then(|()| output.flush())
            .map_err(|_| anyhow!(OUTPUT_FAILED))?;
        let mut input = String::new();
        if read_line(&mut input).is_err() {
            crate::sessions_progress::check()?;
            bail!(INPUT_FAILED);
        }
        crate::sessions_progress::check()?;
        if default == Mode::Resolve {
            let word = input.strip_suffix('\n').unwrap_or(&input);
            let word = word.strip_suffix('\r').unwrap_or(word);
            return (word == "resolve")
                .then_some(Mode::Resolve)
                .ok_or_else(|| anyhow!(CANCELLED));
        }
        if let Some(mode) = parse_mode(&input, allow_share) {
            return Ok(mode);
        }
        if input.is_empty() || matches!(input.as_str(), "\n" | "\r\n") || attempt == 1 {
            break;
        }
    }
    bail!(CANCELLED)
}

/// Returns true for finish. Rollback is never permitted after publication.
pub(crate) fn recovery(yes: bool, can_rollback: bool) -> Result<bool> {
    recovery_with_io(
        yes,
        can_rollback,
        (io::stdin().is_terminal(), io::stdout().is_terminal()),
        crate::sessions_progress::read_line,
        &mut io::stdout().lock(),
    )
}

fn recovery_with_io(
    yes: bool,
    can_rollback: bool,
    terminals: (bool, bool),
    mut read: impl FnMut(&mut String) -> io::Result<usize>,
    output: &mut impl Write,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    let english = crate::sessions_progress::english();
    if !terminals.0 || !terminals.1 {
        bail!(
            "이전 이동 복구를 터미널에서 finish/rollback으로 선택하세요. 스크립트는 --yes를 사용하세요."
        );
    }
    writeln!(
        output,
        "{}",
        if english {
            "The previous move was interrupted. Finish or roll back? (finish/rollback)"
        } else {
            "이전 이동이 중간에 멈췄습니다. 마저 끝낼까요? (finish/rollback)"
        }
    )
    .map_err(|_| anyhow!(OUTPUT_FAILED))?;
    if !can_rollback {
        writeln!(
            output,
            "{}",
            if english {
                "Publication is committed; only finish is safe."
            } else {
                "게시가 확정된 단계라 finish로만 복구할 수 있습니다."
            }
        )
        .map_err(|_| anyhow!(OUTPUT_FAILED))?;
    }
    output.flush().map_err(|_| anyhow!(OUTPUT_FAILED))?;
    let mut line = String::new();
    if read(&mut line).is_err() {
        crate::sessions_progress::check()?;
        bail!(INPUT_FAILED);
    }
    crate::sessions_progress::check()?;
    match line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(&line)
    {
        "finish" => Ok(true),
        "rollback" if can_rollback => Ok(false),
        "rollback" => bail!("게시가 확정된 이동은 되돌릴 수 없습니다. finish로 먼저 복구하세요."),
        _ => bail!(CANCELLED),
    }
}

fn parse_mode(input: &str, allow_share: bool) -> Option<Mode> {
    let word = input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .unwrap_or(input);
    match word {
        "move" => Some(Mode::Move),
        "share" if allow_share => Some(Mode::Share),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::{BufRead, Cursor},
        rc::Rc,
    };

    fn with_text(input: &str, allow_share: bool) -> (Result<Mode>, String) {
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let result = ask_with_io(
            false,
            Mode::Move,
            allow_share,
            "quota-baton sessions import --from SOURCE ID --apply",
            (true, true),
            |line| reader.read_line(line),
            &mut output,
        );
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn typed_resolve_accepts_only_the_exact_word_and_line_ending() {
        for input in ["resolve", "resolve\n", "resolve\r\n"] {
            let (result, output) = with_resolve_text(input);
            assert_eq!(result.unwrap(), Mode::Resolve);
            assert_eq!(output.matches("입력하세요").count(), 1);
            assert!(output.contains("'resolve'"));
            assert!(!output.contains("'move'") && !output.contains("'share'"));
        }
    }

    fn with_resolve_text(input: &str) -> (Result<Mode>, String) {
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let result = ask_with_io(
            false,
            Mode::Resolve,
            false,
            "quota-baton sessions recover ID --resolve-map",
            (true, true),
            |line| reader.read_line(line),
            &mut output,
        );
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn typed_resolve_cancels_different_case_or_extra_input() {
        for input in [
            "",
            "\n",
            "move\n",
            "share\n",
            "cancel\n",
            "Resolve\n",
            "RESOLVE\r\n",
            " resolve\n",
            "resolve \n",
            "resolve\t\n",
            "resolve extra\n",
            "'resolve'\n",
        ] {
            let (result, output) = with_resolve_text(input);
            assert_eq!(result.unwrap_err().to_string(), CANCELLED, "{input:?}");
            assert_eq!(output.matches("입력하세요").count(), 1);
        }
    }

    #[test]
    fn typed_resolve_refuses_either_nonterminal_without_reading() {
        let manual = "quota-baton sessions recover ID --resolve-map";
        for terminals in [(false, false), (false, true), (true, false)] {
            let mut output = Vec::new();
            let result = ask_with_io(
                false,
                Mode::Resolve,
                false,
                manual,
                terminals,
                |_| panic!("nonterminal resolve must not read confirmation"),
                &mut output,
            );
            assert_eq!(result.unwrap_err().to_string(), NO_TERMINAL);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(manual));
            assert!(!output.contains("입력하세요"));
        }
    }

    #[test]
    fn typed_move_accepts_only_the_word_and_line_ending() {
        for input in ["move", "move\n", "move\r\n"] {
            let (result, output) = with_text(input, false);
            assert_eq!(result.unwrap(), Mode::Move);
            assert_eq!(output.matches("입력하세요").count(), 1);
            assert!(!output.contains("share"));
        }
    }

    #[test]
    fn typed_share_requires_permission() {
        for input in ["share", "share\n", "share\r\n"] {
            let (result, output) = with_text(input, true);
            assert_eq!(result.unwrap(), Mode::Share);
            assert_eq!(output.matches("입력하세요").count(), 1);
            assert!(output.contains("move") && output.contains("share"));
            assert_eq!(
                with_text(input, false).0.unwrap_err().to_string(),
                CANCELLED
            );
        }
    }

    #[test]
    fn unknown_input_retries_once_but_enter_and_eof_cancel_immediately() {
        for input in [
            "",
            "\n",
            "\r\n",
            "MOVE\n",
            "Share\n",
            " move\n",
            "move \n",
            "move\t\n",
            "move\r",
            "'move'\n",
            "move share\n",
            "취소\n",
        ] {
            let (result, output) = with_text(input, true);
            assert_eq!(result.unwrap_err().to_string(), CANCELLED);
            let prompts = if matches!(input, "" | "\n" | "\r\n") {
                1
            } else {
                2
            };
            assert_eq!(output.matches("입력하세요").count(), prompts);
        }
    }

    #[test]
    fn parser_does_not_trim_extra_whitespace_or_line_endings() {
        for input in ["move\n\n", "move\r\r\n", "share \n", " share\r\n"] {
            assert_eq!(parse_mode(input, true), None);
        }
    }

    #[test]
    fn either_nonterminal_refuses_before_reading_and_prints_manual_command() {
        let manual = "quota-baton sessions return ID --to HOME";
        for terminals in [(false, false), (false, true), (true, false)] {
            let mut output = Vec::new();
            let result = ask_with_io(
                false,
                Mode::Move,
                true,
                manual,
                terminals,
                |_| panic!("터미널이 없으면 입력을 읽으면 안 됩니다"),
                &mut output,
            );
            assert_eq!(result.unwrap_err().to_string(), NO_TERMINAL);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(manual));
            assert!(!output.contains("입력하세요"));
        }
    }

    struct NoOutput;

    impl Write for NoOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("자동 확인은 안내를 출력하면 안 됩니다")
        }

        fn flush(&mut self) -> io::Result<()> {
            panic!("자동 확인은 출력을 플러시하면 안 됩니다")
        }
    }

    #[test]
    fn yes_returns_caller_default_without_input_or_output() {
        for default in [Mode::Move, Mode::Share] {
            for allow_share in [false, true] {
                let result = ask_with_io(
                    true,
                    default,
                    allow_share,
                    "",
                    (false, false),
                    |_| panic!("자동 확인은 입력을 읽으면 안 됩니다"),
                    &mut NoOutput,
                );
                assert_eq!(result.unwrap(), default);
                assert_eq!(ask(true, default, allow_share, "").unwrap(), default);
            }
        }
    }

    struct FlushMarker {
        output: Vec<u8>,
        flushed: Rc<Cell<bool>>,
    }

    impl Write for FlushMarker {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed.set(true);
            Ok(())
        }
    }

    #[test]
    fn prompt_is_flushed_before_the_single_read() {
        let flushed = Rc::new(Cell::new(false));
        let mut output = FlushMarker {
            output: Vec::new(),
            flushed: Rc::clone(&flushed),
        };
        let result = ask_with_io(
            false,
            Mode::Share,
            true,
            "",
            (true, true),
            |line| {
                assert!(flushed.get());
                line.push_str("move\n");
                Ok(5)
            },
            &mut output,
        );
        assert_eq!(result.unwrap(), Mode::Move);
    }

    #[test]
    fn read_failure_has_a_fixed_korean_diagnostic() {
        let result = ask_with_io(
            false,
            Mode::Move,
            false,
            "",
            (true, true),
            |_| Err(io::Error::other("합성 내부 오류")),
            &mut Vec::new(),
        );
        assert_eq!(result.unwrap_err().to_string(), INPUT_FAILED);
    }

    struct FailedOutput;

    impl Write for FailedOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("합성 내부 오류"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("합성 내부 오류"))
        }
    }

    #[test]
    fn output_failure_cancels_before_reading() {
        let result = ask_with_io(
            false,
            Mode::Move,
            false,
            "",
            (true, true),
            |_| panic!("안내를 출력하지 못하면 입력을 읽으면 안 됩니다"),
            &mut FailedOutput,
        );
        assert_eq!(result.unwrap_err().to_string(), OUTPUT_FAILED);
    }
    #[test]
    fn recovery_offers_exact_choices_but_never_rollback_after_commit() {
        for (word, expected) in [("finish\n", true), ("rollback\r\n", false)] {
            let mut out = Vec::new();
            let answer = recovery_with_io(
                false,
                true,
                (true, true),
                |line| {
                    line.push_str(word);
                    Ok(word.len())
                },
                &mut out,
            )
            .unwrap();
            assert_eq!(answer, expected);
            assert!(String::from_utf8(out).unwrap().contains("finish/rollback"));
        }
        let error = recovery_with_io(
            false,
            false,
            (true, true),
            |line| {
                line.push_str("rollback\n");
                Ok(9)
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("finish로 먼저"));
        for input in ["", "\n", " Finish\n", "finish \n", "finish\n\n"] {
            assert!(
                recovery_with_io(
                    false,
                    true,
                    (true, true),
                    |line| {
                        line.push_str(input);
                        Ok(input.len())
                    },
                    &mut Vec::new()
                )
                .is_err()
            );
        }
    }
    #[test]
    fn recovery_nonterminal_never_reads_and_script_yes_only_finishes() {
        assert!(
            recovery_with_io(
                false,
                true,
                (false, false),
                |_| panic!("must not read"),
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            recovery_with_io(
                true,
                false,
                (false, false),
                |_| panic!("must not read"),
                &mut NoOutput
            )
            .unwrap()
        );
    }
    #[test]
    fn unknown_input_then_exact_move_is_accepted_on_second_prompt() {
        let (result, out) = with_text("typo\nmove\n", true);
        assert_eq!(result.unwrap(), Mode::Move);
        assert_eq!(out.matches("입력하세요").count(), 2);
        assert!(!out.contains('\r') && !out.contains('\u{1b}'));
    }
}
