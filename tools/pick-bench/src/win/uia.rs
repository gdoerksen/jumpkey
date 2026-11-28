//! A tiny UI Automation reader: where is the fixture element with a
//! given AutomationId (an HTML `id` in Chromium) on screen?
//!
//! `check` mode uses it to find which label jumpkey drew over a known
//! button: jumpkey's labels file gives `{label, rect}` with no names,
//! and both sides read the same `BoundingRectangle` property, so the
//! rectangles match exactly. Only the harness's own fixture pages are
//! read, by id; nothing else is looked at.

use anyhow::{Context, Result};
use windows::core::VARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_AutomationIdPropertyId,
};

use super::window::hwnd;

pub struct Uia {
    automation: IUIAutomation,
}

impl Uia {
    pub fn new() -> Result<Self> {
        // SAFETY: COM init on this thread (S_FALSE if already done is fine).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .context("creating the UI Automation client")?;
            Ok(Self { automation })
        }
    }

    /// Screen rect `[x, y, w, h]` of the element under `window` whose
    /// AutomationId is `id`, or `None` if there's no such element.
    pub fn rect_of_id(&self, window: isize, id: &str) -> Option<[i32; 4]> {
        // SAFETY: COM calls on interfaces created on this thread.
        unsafe {
            let root = self.automation.ElementFromHandle(hwnd(window)).ok()?;
            let cond = self
                .automation
                .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(id))
                .ok()?;
            let el = root.FindFirst(TreeScope_Descendants, &cond).ok()?;
            let r = el.CurrentBoundingRectangle().ok()?;
            (r.right > r.left && r.bottom > r.top).then_some([
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
            ])
        }
    }
}
