//! Finding the scrollable areas of the target window (UI Automation).
//!
//! Two passes, from the step 4 spike (#34):
//! - **First area** ([`first_area`], 2–5 ms): the nearest scrollable
//!   ancestor of the element under the cursor (when the cursor is over
//!   the window), else of the focused element, else the window's client
//!   area.
//! - **All areas** ([`all_areas`], 11–58 ms on the bench pages): one
//!   provider-side `FindAll(Descendants, IsScrollPatternAvailable)` with
//!   the bounds and scrollable flags cached.
//!
//! Chromium only exposes scroll containers that are in its accessibility
//! tree (a role, label or `tabindex`); plain-div scrollers aren't found.
//! The wheel still scrolls them when the pointer is over them.
//!
//! Every call here can block on a slow app, so they run on a worker with
//! its own [`UIAutomation`], never on the main or hook thread.

use uiautomation::types::{Handle, Point, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClientRect, GetWindowRect, WindowFromPoint, GA_ROOT,
};

use crate::model::Bounds;
use crate::scroll::{Area, FirstFrom};

/// Ancestors walked up from the point or focus before giving up.
const MAX_WALK_UP: usize = 40;

/// The window's client area in screen coordinates.
pub fn client_bounds(hwnd: HWND) -> Bounds {
    let mut r = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        if GetClientRect(hwnd, &mut r).is_err() || !ClientToScreen(hwnd, &mut origin).as_bool() {
            let _ = GetWindowRect(hwnd, &mut r);
            return Bounds {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            };
        }
    }
    Bounds {
        x: origin.x,
        y: origin.y,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

/// The window's outer rectangle in screen coordinates.
pub fn window_bounds(hwnd: HWND) -> Bounds {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    Bounds {
        x: r.left,
        y: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

/// True when `pt` is over `hwnd` (or one of its children), not over a
/// window covering it.
pub fn cursor_over(hwnd: HWND, pt: POINT) -> bool {
    unsafe {
        let under = WindowFromPoint(pt);
        !under.0.is_null() && GetAncestor(under, GA_ROOT) == hwnd
    }
}

fn prop_bool(el: &UIElement, p: UIProperty, cached: bool) -> bool {
    let v = if cached {
        el.get_cached_property_value(p)
    } else {
        el.get_property_value(p)
    };
    v.ok().and_then(|v| v.try_into().ok()).unwrap_or(false)
}

fn area_of(el: &UIElement, cached: bool) -> Option<Area> {
    let r = if cached {
        el.get_cached_bounding_rectangle()
    } else {
        el.get_bounding_rectangle()
    }
    .ok()?;
    let area = Area {
        bounds: Bounds {
            x: r.get_left(),
            y: r.get_top(),
            width: r.get_width(),
            height: r.get_height(),
        },
        can_h: prop_bool(el, UIProperty::ScrollHorizontallyScrollable, cached),
        can_v: prop_bool(el, UIProperty::ScrollVerticallyScrollable, cached),
    };
    (area.can_h || area.can_v).then_some(area)
}

/// The nearest ancestor-or-self of `el` that can scroll, as an element.
pub fn scrollable_ancestor(uia: &UIAutomation, el: UIElement) -> Option<UIElement> {
    let walker = uia.get_raw_view_walker().ok()?;
    let mut el = el;
    for _ in 0..MAX_WALK_UP {
        if prop_bool(&el, UIProperty::IsScrollPatternAvailable, false)
            && area_of(&el, false).is_some()
        {
            return Some(el);
        }
        el = walker.get_parent(&el).ok()?;
    }
    None
}

/// The scrollable element under a screen point (for jumps).
pub fn scrollable_at(uia: &UIAutomation, x: i32, y: i32) -> Option<UIElement> {
    let el = uia.element_from_point(Point::new(x, y)).ok()?;
    scrollable_ancestor(uia, el)
}

/// The session's instant first area.
pub fn first_area(uia: &UIAutomation, hwnd: HWND, cursor: POINT) -> (Area, FirstFrom) {
    if cursor_over(hwnd, cursor) {
        if let Some(a) = scrollable_at(uia, cursor.x, cursor.y).and_then(|el| area_of(&el, false)) {
            return (a, FirstFrom::Point);
        }
    }
    if let Some(a) = uia
        .get_focused_element()
        .ok()
        .and_then(|el| scrollable_ancestor(uia, el))
        .and_then(|el| area_of(&el, false))
    {
        // Focus can be in another window (a popup); only trust it inside
        // this one.
        let w = window_bounds(hwnd);
        let (cx, cy) = a.bounds.center();
        if cx >= w.x && cy >= w.y && cx < w.x + w.width && cy < w.y + w.height {
            return (a, FirstFrom::Focus);
        }
    }
    (
        Area {
            bounds: client_bounds(hwnd),
            can_h: false,
            can_v: true,
        },
        FirstFrom::Window,
    )
}

/// Every scrollable area UI Automation reports in the window (unfiltered;
/// see [`crate::scroll::arrange_areas`]).
pub fn all_areas(uia: &UIAutomation, hwnd: HWND) -> Vec<Area> {
    let run = || -> uiautomation::Result<Vec<Area>> {
        let req = uia.create_cache_request()?;
        for p in [
            UIProperty::BoundingRectangle,
            UIProperty::ScrollHorizontallyScrollable,
            UIProperty::ScrollVerticallyScrollable,
        ] {
            req.add_property(p)?;
        }
        let root = uia.element_from_handle(Handle::from(hwnd.0 as isize))?;
        let cond = uia.create_property_condition(
            UIProperty::IsScrollPatternAvailable,
            Variant::from(true),
            None,
        )?;
        let found = root
            .find_all_build_cache(TreeScope::Descendants, &cond, &req)
            .unwrap_or_default();
        Ok(found.iter().filter_map(|e| area_of(e, true)).collect())
    };
    run().unwrap_or_else(|e| {
        tracing::debug!(error = ?e, "scroll area discovery failed");
        Vec::new()
    })
}
