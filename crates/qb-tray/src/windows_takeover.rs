//! R6 UI controller. Every async completion is owned by a monotonic view ticket.
//! No source credentials are opened by the tray: only core's allowlisted DTO.
use super::*;
use crate::client::TakeoverInspect;

#[derive(Clone)]
struct Inspected {
    ticket: usize,
    provider: String,
    path: String,
    inspect: TakeoverInspect,
}
static INSPECTED: Mutex<Option<Inspected>> = Mutex::new(None);

#[cfg(all(debug_assertions, feature = "testkit"))]
fn sandbox_path(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path.components().any(|c| c.as_os_str() == ".sandbox")
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}
#[cfg(all(debug_assertions, feature = "testkit"))]
pub(super) fn synthetic_picker_ready() -> bool {
    !FIXTURE_MODE.load(Ordering::Acquire)
        && std::env::var_os("QB_HOME").is_some_and(|value| sandbox_path(&PathBuf::from(value)))
        && std::env::var_os("QB_TRAY_TEST_TAKEOVER_FOLDER").is_some_and(|value| {
            if value == "cancel" {
                return true;
            }
            sandbox_path(&PathBuf::from(value))
        })
}

pub(super) fn invalidate() {
    *INSPECTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
pub(super) fn card(state: &str) -> Overlay {
    // Match publication/set_view order. Never render a payload from another owner.
    let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
    let data = INSPECTED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(data) = data.as_ref().filter(|d| d.ticket == view.owner()) else {
        return Overlay::TakeoverCard {
            body: if state == "takeover-pending" {
                i18n::static_text("가져오는 중입니다. 원본 파일을 정리하고 토큰 갱신을 확인합니다.")
            } else {
                i18n::static_text("계정 정보를 확인하고 있습니다. 잠시 기다리세요.")
            }
            .into(),
            confirming: false,
            ready: false,
            replace: false,
        };
    };
    let confirming = matches!(state, "takeover-confirm" | "takeover-pending");
    let body = if confirming {
        format!(
            "{}{}\n\n{}",
            data.inspect.warning(),
            if data.inspect.existing_id.is_some() {
                i18n::static_text("\n기존 계정을 교체합니다. 계정 ID, 연결 및 기록은 유지됩니다.")
            } else {
                ""
            },
            data.inspect.identity(v2::provider_label(&data.provider))
        )
    } else {
        data.inspect.identity(v2::provider_label(&data.provider))
    };
    Overlay::TakeoverCard {
        body,
        confirming,
        ready: data.inspect.can_confirm() && state != "takeover-pending",
        replace: data.inspect.existing_id.is_some(),
    }
}

/// Native dialog is modal on the UI apartment. Only selecting a directory occurs
/// here; AdminClient construction, credential loading and HTTP stay on Worker.
fn folder(hwnd: HWND) -> Result<Option<String>> {
    // A fixture must never open a production picker, including release builds.
    anyhow::ensure!(
        !FIXTURE_MODE.load(Ordering::Acquire),
        "takeover requires a synthetic live core or production mode"
    );
    #[cfg(not(all(debug_assertions, feature = "testkit")))]
    anyhow::ensure!(
        std::env::var_os("QB_TRAY_TEST_TAKEOVER_FOLDER").is_none(),
        "synthetic picker hook unavailable"
    );
    #[cfg(all(debug_assertions, feature = "testkit"))]
    {
        // Narrow synthetic hook: never fall through to a real picker in fixture mode.
        // An explicit hook value works with a synthetic live core as well.
        if let Some(value) = std::env::var_os("QB_TRAY_TEST_TAKEOVER_FOLDER") {
            anyhow::ensure!(
                synthetic_picker_ready(),
                "synthetic folder must be absolute and under .sandbox"
            );
            return Ok(if value == "cancel" {
                None
            } else {
                Some(value.to_string_lossy().into_owned())
            });
        }
        anyhow::ensure!(
            std::env::var_os("QB_HOME").is_none_or(|p| !sandbox_path(&PathBuf::from(p))),
            "synthetic picker hook required"
        );
    }
    use windows::Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
        UI::Shell::{
            FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
            FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    };
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(
            dialog.GetOptions()?
                | FOS_PICKFOLDERS
                | FOS_FORCEFILESYSTEM
                | FOS_PATHMUSTEXIST
                | FOS_DONTADDTORECENT,
        )?;
        let title: Vec<u16> = i18n::text("가져올 계정의 홈 폴더 선택")
            .encode_utf16()
            .chain([0])
            .collect();
        dialog.SetTitle(PCWSTR(title.as_ptr()))?;
        if let Err(error) = dialog.Show(Some(hwnd)) {
            if error.code() == windows::core::HRESULT(0x800704c7u32 as i32) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = name.to_string();
        CoTaskMemFree(Some(name.0.cast()));
        Ok(Some(path?))
    }
}

pub(super) fn select(hwnd: HWND, provider: &str) {
    if SHUTDOWN_PENDING.load(Ordering::Acquire) {
        return;
    }
    let ticket = set_view(Some("takeover-picking".into()));
    let path = match folder(hwnd) {
        Ok(Some(path)) => path,
        Ok(None) => {
            picker_done(ticket, "takeover-providers");
            return;
        }
        Err(_) => {
            picker_done(ticket, "error:폴더를 선택하지 못했습니다. 다시 시도하세요.");
            return;
        }
    };
    // A picker runs a nested native message loop: dismissal or shutdown can win.
    {
        let mut view = view_state().lock().unwrap_or_else(|e| e.into_inner());
        if !view.publish(ticket, Some("takeover-inspecting".into())) {
            return;
        }
    }
    let provider = provider.to_owned();
    let hwnd = hwnd.0 as isize;
    submit_job(async move {
        {
            let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
            if view.owner() != ticket {
                return;
            }
        }
        let result: Result<TakeoverInspect> = async {
            let client = crate::client::AdminClient::from_home()?;
            Ok(client.takeover_inspect(&provider, &path).await?)
        }
        .await;
        // Publishing both the data and the view while holding the ticket lock
        // prevents cancellation/newer inspection from resurrecting this result.
        let mut view = view_state().lock().unwrap_or_else(|e| e.into_inner());
        if view.owner() != ticket {
            return;
        }
        match result {
            Ok(inspect) => {
                *INSPECTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Inspected {
                    ticket,
                    provider,
                    path,
                    inspect,
                });
                view.publish(ticket, Some("takeover-inspected".into()));
            }
            Err(_) => {
                view.publish(ticket, Some("error:계정을 검사하지 못했습니다. 원래 앱과 폴더를 확인하고 다시 시도하세요.".into()));
            }
        }
        drop(view);
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut c_void)),
                STATE_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
    });
}

fn picker_done(ticket: usize, state: &str) {
    view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .publish(ticket, Some(state.into()));
}
// Advance the owner and its payload atomically, always view -> INSPECTED.
// In particular, pending commits retain their confirmed identity/warning.
fn advance(expected: &str, next: &str) -> Option<Inspected> {
    let mut view = view_state().lock().unwrap_or_else(|e| e.into_inner());
    let mut data = INSPECTED.lock().unwrap_or_else(|e| e.into_inner());
    advance_owned(&mut view, &mut data, expected, next)
}
fn advance_owned(
    view: &mut view_ownership::ViewOwnership,
    data: &mut Option<Inspected>,
    expected: &str,
    next: &str,
) -> Option<Inspected> {
    let inspected = data.as_mut()?;
    if !accepts(view.owner(), view.as_deref(), expected, inspected) {
        return None;
    }
    inspected.ticket = view.set(Some(next.into()));
    Some(inspected.clone())
}
fn accepts(owner: usize, state: Option<&str>, expected: &str, data: &Inspected) -> bool {
    owner == data.ticket && state == Some(expected) && data.inspect.can_confirm()
}
pub(super) fn next() {
    if advance("takeover-inspected", "takeover-confirm").is_none() {
        return;
    }
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.popup_offset = 0.;
        i.focus = Some(HitTarget::CancelDialog);
    });
}
pub(super) fn commit(hwnd: HWND) {
    let Some(data) = advance("takeover-confirm", "takeover-pending") else {
        return;
    };
    let ticket = data.ticket;
    INPUT.with(|i| i.borrow_mut().popup_offset = 0.);
    let hwnd = hwnd.0 as isize;
    submit_job(async move {
        {
            let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
            if view.owner() != ticket {
                return;
            }
        }
        let result: Result<crate::client::TakeoverCommit> = async {
            let client = crate::client::AdminClient::from_home()?;
            Ok(client
                .takeover_commit(
                    &data.provider,
                    &data.path,
                    &data.inspect,
                    data.inspect.existing_id.is_some(),
                )
                .await?)
        }
        .await;
        // Commit may finish after dismissal, but cannot change the newer UI.
        // Never retry: a lost response may already have retired the source.
        let message = match result {
            Ok(result) if result.refresh.ok => {
                i18n::static_text("가져오기를 마쳤고 토큰 갱신을 확인했습니다.")
            }
            Ok(_) => i18n::static_text(
                "가져왔지만 토큰 갱신을 확인하지 못했습니다. 계정 상태를 확인하세요.",
            ),
            Err(_) => i18n::static_text(
                "가져오기를 확인하지 못했습니다. 계정 목록과 원본 폴더를 확인하세요. 앱 실행, 최근 파일 변경 또는 기존 계정 충돌이면 다시 검사하세요. 자동 재시도하지 않습니다.",
            ),
        };
        let mut view = view_state().lock().unwrap_or_else(|e| e.into_inner());
        view.publish(ticket, Some(format!("takeover-result:{message}")));
        drop(view);
        update_status(false, None);
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut c_void)),
                STATE_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r6_atomic_transition_keeps_pending_identity_and_rejects_duplicate_commit() {
        let mut view = view_ownership::ViewOwnership::default();
        let ticket = view.set(Some("takeover-inspected".into()));
        let mut data = Some(Inspected {
            ticket,
            provider: "codex".into(),
            path: "C:/synthetic/home".into(),
            inspect: serde_json::from_value(serde_json::json!({"source_path":"C:/synthetic/home",
                "owner_alive":false,"inspect_digest":"digest","existing_id":"same-id"}))
            .unwrap(),
        });
        let confirmed = advance_owned(
            &mut view,
            &mut data,
            "takeover-inspected",
            "takeover-confirm",
        )
        .unwrap();
        assert_ne!(confirmed.ticket, ticket);
        assert!(
            advance_owned(
                &mut view,
                &mut data,
                "takeover-inspected",
                "takeover-confirm"
            )
            .is_none()
        );
        let pending =
            advance_owned(&mut view, &mut data, "takeover-confirm", "takeover-pending").unwrap();
        assert_eq!(data.as_ref().unwrap().ticket, view.owner());
        assert_eq!(pending.inspect.existing_id.as_deref(), Some("same-id"));
        assert!(
            advance_owned(&mut view, &mut data, "takeover-confirm", "takeover-pending").is_none()
        );
        view.set(Some("takeover-providers".into()));
        assert!(!view.publish(pending.ticket, Some("takeover-result:late".into())));
        // A nested picker cancellation/failure is also just a stale publication.
        assert!(!view.publish(ticket, Some("takeover-providers".into())));
        assert!(
            advance_owned(
                &mut view,
                &mut data,
                "takeover-providers",
                "takeover-confirm"
            )
            .is_none()
        );
    }
    #[cfg(all(debug_assertions, feature = "testkit"))]
    #[test]
    fn r6_synthetic_picker_paths_reject_relative_and_traversal_without_io() {
        for path in [
            ".sandbox/home",
            "C:/project/.sandbox/../live",
            "C:/project/live",
        ] {
            assert!(!sandbox_path(std::path::Path::new(path)), "{path}");
        }
        assert!(sandbox_path(std::path::Path::new(
            "C:/project/.sandbox/home"
        )));
    }
    #[test]
    fn dismissed_and_reopened_inspections_cannot_authorize_replace_or_publish_late_results() {
        let mut view = view_ownership::ViewOwnership::default();
        let ticket = view.set(Some("takeover-confirm".into()));
        let data = Inspected {
            ticket,
            provider: "codex".into(),
            path: "C:/synthetic/home".into(),
            inspect: serde_json::from_value(
                serde_json::json!({"email":"x@example.invalid", "source_path":"C:/synthetic/home",
                "owner_alive":false,"inspect_digest":"synthetic-digest","existing_id":"same-id"}),
            )
            .unwrap(),
        };
        assert!(accepts(
            view.owner(),
            view.as_deref(),
            "takeover-confirm",
            &data
        ));
        assert!(!accepts(
            view.owner(),
            view.as_deref(),
            "takeover-inspected",
            &data
        ));
        view.set(None);
        assert!(!accepts(
            view.owner(),
            view.as_deref(),
            "takeover-confirm",
            &data
        ));
        assert!(!view.publish(ticket, Some("takeover-confirm".into())));
        view.set(Some("takeover-confirm".into()));
        assert!(!accepts(
            view.owner(),
            view.as_deref(),
            "takeover-confirm",
            &data
        ));
        assert!(!view.publish(ticket, Some("takeover-result:stale".into())));
    }
}
