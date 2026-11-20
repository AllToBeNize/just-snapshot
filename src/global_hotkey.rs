//! The global shortcuts (capture, its optional second key, capture window and
//! last capture), registered on the tray window. A shortcut held by another
//! program stays configured and is reported as a conflict instead of failing
//! startup.

use std::cell::RefCell;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT},
};

use crate::hotkey::{Global, Shortcut};

#[derive(Clone, Copy)]
struct Slot {
    shortcut: Shortcut,
    action: Global,
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

/// Replaces every global shortcut. Returns the ones another program holds.
pub unsafe fn set(window: HWND, shortcuts: &[(Shortcut, Global)]) -> Vec<Shortcut> {
    let unchanged = SLOTS.with_borrow(|slots| {
        slots.len() == shortcuts.len()
            && slots.iter().zip(shortcuts).all(|(slot, (shortcut, action))| {
                slot.registered && slot.shortcut == *shortcut && slot.action == *action
            })
    });
    if !unchanged {
        unregister_all(window);
        let slots = shortcuts
            .iter()
            .enumerate()
            .map(|(index, &(shortcut, action))| Slot {
                shortcut,
                action,
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

/// What the hotkey with this id does.
pub fn action(id: usize) -> Option<Global> {
    SLOTS.with_borrow(|slots| slots.get(id.checked_sub(1)?).map(|slot| slot.action))
}

/// The configured capture shortcuts, main first.
pub fn shortcuts() -> Vec<Shortcut> {
    SLOTS.with_borrow(|slots| {
        slots
            .iter()
            .filter(|slot| slot.action == Global::Capture)
            .map(|slot| slot.shortcut)
            .collect()
    })
}

/// The shortcut for `action`, if one is configured.
pub fn shortcut(action: Global) -> Option<Shortcut> {
    SLOTS.with_borrow(|slots| slots.iter().find(|slot| slot.action == action).map(|slot| slot.shortcut))
}

/// Whether Snapshot currently holds `shortcut` as a global key.
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

/// "Ctrl+Alt+A / F1": the capture keys, for menus and tooltips.
pub fn label() -> String {
    shortcuts()
        .iter()
        .map(|shortcut| shortcut.label())
        .collect::<Vec<_>>()
        .join(" / ")
}
