pub fn set_console_utf8() {
    // 콘솔에 연결된 CLI 출력은 한국어를 UTF-8로 표시한다. 리디렉션은 원래 UTF-8 바이트를 쓴다.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }
}

pub fn set_process_priority() -> anyhow::Result<()> {
    // 릴레이가 CPU 부하로 밀리지 않도록 코어 우선순위를 높인다.
    unsafe {
        use windows::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS)?;
    }
    Ok(())
}

pub fn home() -> anyhow::Result<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("QB_HOME") {
        return Ok(value.into());
    }
    Ok(std::path::PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA is missing"))?,
    )
    .join("quota-baton"))
}
