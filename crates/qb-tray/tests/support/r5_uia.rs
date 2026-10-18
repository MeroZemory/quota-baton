//! Queries only the synthetic tray's owned HWND through the real UIA client.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use windows::Win32::{
    Foundation::HWND,
    System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    },
    UI::Accessibility::{
        CUIAutomation8, IUIAutomation2, TreeScope_Descendants, UIA_ListItemControlTypeId,
        UIA_TextControlTypeId,
    },
};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn assert_names(hwnd: HWND, rows: &[String], summaries: &[String]) -> Result<Value> {
    ensure!(!hwnd.0.is_null(), "owned tray HWND required");
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _apartment = Apartment;
    let automation: IUIAutomation2 =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        automation.SetConnectionTimeout(1000)?;
        automation.SetTransactionTimeout(1000)?;
    }
    let root = unsafe { automation.ElementFromHandle(hwnd)? };
    let condition = unsafe { automation.CreateTrueCondition()? };
    let elements = unsafe { root.FindAll(TreeScope_Descendants, &condition)? };
    let count = unsafe { elements.Length()? };
    ensure!(
        (1..=512).contains(&count),
        "unexpected owned UIA descendant count: {count}"
    );
    let mut row_names = Vec::new();
    let mut summary_names = Vec::new();
    for i in 0..count {
        let item = unsafe { elements.GetElement(i)? };
        let kind = unsafe { item.CurrentControlType()? };
        let name = unsafe { item.CurrentName()? }.to_string();
        if kind == UIA_ListItemControlTypeId {
            row_names.push(name);
        } else if kind == UIA_TextControlTypeId {
            summary_names.push(name);
        }
    }
    for name in rows {
        ensure!(
            row_names.contains(name),
            "real UIA is missing a complete visible ListItem name"
        );
    }
    for name in summaries {
        ensure!(
            summary_names.contains(name),
            "real UIA is missing a complete summary Text name"
        );
    }
    ensure!(
        !rows.is_empty() && summaries.len() == 2,
        "non-vacuous UIA acceptance requires rows and both summaries"
    );
    Ok(
        json!({"kind":"live-UIA-owned-HWND","descendants":count,"expected_rows":rows,"expected_summaries":summaries,"list_items":row_names,"text":summary_names}),
    )
}
