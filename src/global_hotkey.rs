//! The global capture shortcuts (main plus optional second), registered on the
//! tray window. A shortcut held by another program stays configured and is
//! reported as a conflict instead of failing startup.

use std::cell::RefCell;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT},
};

use crate::hotkey::Shortcut;

#[derive(Clone, Copy)]
struct Slot {
    shortcut: Shortcut,
    registered: bool,
}

thread_local! {
    static SLOTS: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
}

/// Hotkey id of slot `index`; ids start at 1.
fn id(index: usize) -> i32 {
    index as i32 + 1
}

unsafe fn register(window: HWND, index: usize, shortcut: Shortcut) -> bool {
    RegisterHotKey(
        window,
        id(index),
        shortcut.modifiers | MOD_NOREPEAT,
        shortcut.key,
    ) != 0
}

/// Replaces every capture shortcut. Returns the ones another program holds.
pub unsafe fn set(window: HWND, shortcuts: &[Shortcut]) -> Vec<Shortcut> {
    let unchanged = SLOTS.with_borrow(|slots| {
        slots.len() == shortcuts.len()
            && slots
                .iter()
                .zip(shortcuts)
                .all(|(slot, shortcut)| slot.registered && slot.shortcut == *shortcut)
    });
    if !unchanged {
        unregister_all(window);
        let slots = shortcuts
            .iter()
            .enumerate()
            .map(|(index, &shortcut)| Slot {
                shortcut,
                registered: register(window, index, shortcut),
            })
            .collect();
        SLOTS.set(slots);
    }
    conflicts()
}

pub unsafe fn unregister_all(window: HWND) {
    SLOTS.with_borrow_mut(|slots| {
        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.registered {
                UnregisterHotKey(window, id(index));
                slot.registered = false;
            }
        }
    });
}

/// Tries again to take shortcuts another program held. True if any changed.
pub unsafe fn retry(window: HWND) -> bool {
    SLOTS.with_borrow_mut(|slots| {
        let mut changed = false;
        for (index, slot) in slots.iter_mut().enumerate() {
            if !slot.registered && register(window, index, slot.shortcut) {
                slot.registered = true;
                changed = true;
            }
        }
        changed
    })
}

pub fn is_capture(id: usize) -> bool {
    SLOTS.with_borrow(|slots| (1..=slots.len()).contains(&id))
}

/// The configured shortcuts, main first.
pub fn shortcuts() -> Vec<Shortcut> {
    SLOTS.with_borrow(|slots| slots.iter().map(|slot| slot.shortcut).collect())
}

/// Whether Snapshot currently holds `shortcut` as a capture key.
pub fn holds(shortcut: Shortcut) -> bool {
    SLOTS.with_borrow(|slots| {
        slots
            .iter()
            .any(|slot| slot.registered && slot.shortcut == shortcut)
    })
}

/// Configured shortcuts that another program holds.
pub fn conflicts() -> Vec<Shortcut> {
    SLOTS.with_borrow(|slots| {
        slots
            .iter()
            .filter(|slot| !slot.registered)
            .map(|slot| slot.shortcut)
            .collect()
    })
}

/// "Ctrl+Alt+A / F1" for menus and tooltips.
pub fn label() -> String {
    shortcuts()
        .iter()
        .map(|shortcut| shortcut.label())
        .collect::<Vec<_>>()
        .join(" / ")
}
