use super::super::*;
use super::clipboard::session;

fn label(tip: Option<(RECT, Vec<u16>)>) -> String {
    let text = tip.map(|(_, label)| String::from_utf16_lossy(&label)).unwrap_or_default();
    text.trim_end_matches('\0').to_string()
}

#[test]
fn image_search_progress_sits_over_its_button_until_it_ends() {
    unsafe {
        let instance = GetModuleHandleW(null());
        let _ = register(instance);
        let window = session(instance);
        SESSION.with_borrow_mut(|session| session.as_mut().unwrap().toolbar_hover = Some(Action::Copy));
        let hovered = SESSION.with_borrow(|session| label(current_tip(session.as_ref().unwrap())));
        assert!(hovered.starts_with("复制"), "{hovered}");

        set_search(window, Some("正在用 Google 识图…".into()));
        let (tip, button) = SESSION.with_borrow(|session| {
            let selection = session.as_ref().unwrap();
            (current_tip(selection), selection.toolbar.unwrap().button_bounds(Action::SearchImage).unwrap())
        });
        let bounds = tip.as_ref().unwrap().0;
        let center = (button.left + button.right) / 2;
        assert!(bounds.left <= center && center <= bounds.right, "{} to {} over {center}", bounds.left, bounds.right);
        assert_eq!(label(tip), "正在用 Google 识图…", "progress replaces the hover tooltip");
        // A second click while it runs does not ask again.
        search_image(window);

        set_search(window, None);
        let after = SESSION.with_borrow(|session| label(current_tip(session.as_ref().unwrap())));
        assert_eq!(after, hovered);
        DestroyWindow(window);
    }
}
