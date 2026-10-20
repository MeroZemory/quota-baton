//! One typed confirmation after the caller prints the complete session plan.

use anyhow::{Result, anyhow, bail};
use std::io::{self, BufRead, IsTerminal, Write};

const CANCELLED: &str = "확인 입력이 일치하지 않아 취소했습니다.";
const NO_TERMINAL: &str = "표준 입력과 출력이 모두 터미널이어야 합니다. 수동 명령을 실행해 주세요.";
const OUTPUT_FAILED: &str = "확인 안내를 출력하지 못했습니다.";
const INPUT_FAILED: &str = "확인 입력을 읽지 못해 취소했습니다.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Move,
    Share,
}

/// Ask once; cancellation and I/O failure return an error without retrying.
/// `yes` selects the caller's default without inspecting terminals or reading.
pub(crate) fn ask(yes: bool, default: Mode, allow_share: bool, manual: &str) -> Result<Mode> {
    if yes {
        return Ok(default);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let terminals = (stdin.is_terminal(), stdout.is_terminal());
    ask_with_io(
        false,
        default,
        allow_share,
        manual,
        terminals,
        |line| stdin.lock().read_line(line),
        &mut stdout.lock(),
    )
}

fn ask_with_io(
    yes: bool,
    default: Mode,
    allow_share: bool,
    manual: &str,
    terminals: (bool, bool),
    read_line: impl FnOnce(&mut String) -> io::Result<usize>,
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

    let prompt = if allow_share {
        "계속하려면 'move' 또는 'share'를 정확히 입력하세요: "
    } else {
        "계속하려면 'move'를 정확히 입력하세요: "
    };
    output
        .write_all(prompt.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|_| anyhow!(OUTPUT_FAILED))?;

    let mut input = String::new();
    read_line(&mut input).map_err(|_| anyhow!(INPUT_FAILED))?;
    parse_mode(&input, allow_share).ok_or_else(|| anyhow!(CANCELLED))
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
    use std::{cell::Cell, io::Cursor, rc::Rc};

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
    fn other_input_and_eof_cancel_without_another_prompt() {
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
            assert_eq!(output.matches("입력하세요").count(), 1);
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
}
