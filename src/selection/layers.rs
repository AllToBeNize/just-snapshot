//! Stepping through nested auto-selection layers with the mouse side
//! buttons (or the wheel, or Alt+←/→): control, panel, window, screen. Moving
//! the mouse drops the choice and automatic snapping takes over again.

use super::*;

/// How far the pointer may drift before the chosen layer is let go.
const DRIFT: i32 = 3;

pub(super) struct LayerPick {
    anchor: POINT,
    /// Smallest first, in overlay coordinates.
    rects: Vec<RECT>,
    index: usize,
}

impl LayerPick {
    /// "2/5" for the size badge.
    pub(super) fn label(&self) -> String {
        format!("{}/{}", self.index + 1, self.rects.len())
    }
}

fn drifted(anchor: POINT, point: POINT) -> bool {
    (anchor.x - point.x).abs() > DRIFT || (anchor.y - point.y).abs() > DRIFT
}

/// Keeps a chosen layer while the pointer stays put. Returns true when hover
/// must not re-snap; drops the choice once the pointer has moved.
pub(super) fn hold_layer(selection: &mut Selection, point: POINT) -> bool {
    match &selection.layer {
        Some(pick) if !drifted(pick.anchor, point) => true,
        Some(_) => {
            selection.layer = None;
            // Snap again even if the pointer comes back to the same pixel.
            selection.pointer = None;
            selection.target = None;
            false
        }
        None => false,
    }
}

/// Every layer under `point`: the controls and child windows the probe found,
/// the snapped rectangle, the window and its monitor.
unsafe fn candidates(selection: &Selection, point: POINT) -> Vec<RECT> {
    let desktop = selection.desktop.bounds;
    let screen = POINT { x: point.x + desktop.left, y: point.y + desktop.top };
    let mut rects: Vec<RECT> = selection.element_layers.iter().filter_map(|rect| overlay_rect(*rect, desktop)).collect();
    if let Some(window) = selection
        .targets
        .iter()
        .find(|target| Some(target.id) == selection.target && contains(target.rect, screen))
        .and_then(|target| overlay_rect(target.rect, desktop))
    {
        rects.push(window);
    }
    if let Some(monitor) = monitor_info(screen).and_then(|info| overlay_rect(info.rcMonitor, desktop)) {
        rects.push(monitor);
    }
    if !outline_empty(selection.rect) {
        rects.push(selection.rect);
    }
    targets::layers(rects, point)
}

/// One layer out (`larger`) or in. False when not choosing automatically.
pub(super) unsafe fn step_layer(window: HWND, larger: bool) -> bool {
    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor);
    ScreenToClient(window, &mut cursor);
    step_layer_at(window, cursor, larger)
}

pub(super) unsafe fn step_layer_at(window: HWND, point: POINT, larger: bool) -> bool {
    let change = SESSION.with_borrow_mut(|session| {
        let selection = session.as_mut()?;
        if !selection.automatic || selection.drag.is_some() {
            return None;
        }
        if selection.layer.as_ref().is_none_or(|pick| drifted(pick.anchor, point)) {
            let rects = candidates(selection, point);
            if rects.is_empty() {
                return None;
            }
            // Start from the layer automatic snapping picked.
            let current = selection.rect;
            let index = rects
                .iter()
                .enumerate()
                .min_by_key(|(_, rect)| {
                    (rect.left - current.left).abs()
                        + (rect.top - current.top).abs()
                        + (rect.right - current.right).abs()
                        + (rect.bottom - current.bottom).abs()
                })
                .map_or(0, |(index, _)| index);
            selection.layer = Some(LayerPick { anchor: point, rects, index });
        }
        let pick = selection.layer.as_mut()?;
        pick.index = if larger { (pick.index + 1).min(pick.rects.len() - 1) } else { pick.index.saturating_sub(1) };
        let previous = selection.rect;
        selection.rect = pick.rects[pick.index];
        Some((previous, selection.rect, selection.canvas))
    });
    let Some((old, new, canvas)) = change else {
        return false;
    };
    invalidate_change(window, old, new, canvas);
    // The badge shows the layer number even when the rectangle stays.
    let badge = SESSION.with_borrow(|session| {
        let selection = session.as_ref()?;
        size_badge(new, selection.desktop.bounds, selection.canvas)
    });
    if let Some(badge) = badge {
        InvalidateRect(window, &badge, 0);
    }
    true
}
