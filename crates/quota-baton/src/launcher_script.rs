//! Generated Codex launcher shared by wiring and synthetic console tests.
use std::path::Path;

/// Format the launcher after the caller has validated the helper command path.
pub(crate) fn codex_launcher_bytes(helper: &Path) -> Vec<u8> {
    // Invoke the native helper directly. A redundant cmd /c or CALL would
    // reparse percent expressions inside the forwarded arguments.
    format!(
        concat!(
            "@echo off\r\n",
            "setlocal DisableDelayedExpansion\r\n",
            "rem quota-baton dedicated Codex home\r\n",
            "set \"ERRORLEVEL=\"\r\n",
            "for /f \"tokens=2 delims=:\" %%P in ('chcp') do set \"QB_CODEPAGE=%%P\"\r\n",
            "chcp 65001 >nul\r\n",
            "for %%I in (.) do set \"QB_LAUNCHER_FOLDER=%%~nxI\"\r\n",
            "setlocal EnableDelayedExpansion\r\n",
            "title codex-qb · !QB_LAUNCHER_FOLDER!\r\n",
            "endlocal\r\n",
            "set \"CODEX_HOME=%USERPROFILE%\\.codex-acct\\qb\"\r\n",
            "\"{}\" launch-client --client codex --command \"%APPDATA%\\npm\\codex.cmd\" -- %*\r\n",
            "set \"QB_EXIT=%errorlevel%\"\r\n",
            "chcp %QB_CODEPAGE% >nul\r\n",
            "endlocal & exit /b %QB_EXIT%\r\n",
        ),
        helper.display()
    )
    .into_bytes()
}
