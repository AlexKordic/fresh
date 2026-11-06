//! Pointer input for a popup placed by a pixel-measured frontend. The retained
//! subtree remains the only list/text/scroll handler; rejected input is consumed
//! here and never becomes a document click.

use super::{
    wire::{Field, Reader, WireError},
    MouseInput,
};
use crate::app::Editor;
use crossterm::event::{MouseEvent, MouseEventKind};

#[cfg(test)]
#[path = "popup_pointer_tests.rs"]
mod tests;

pub(super) struct Capture {
    pub instance: u64,
    key: fresh_ui::Key,
    owner: crate::view::scene::PopupSourceAnchor,
}

pub(super) fn retire(editor: &mut Editor, capture: &mut Option<Capture>) {
    if let Some(capture) = capture.take() {
        if let Some(ui) = editor.shell_ui.as_mut() {
            ui.cancel_pointer_subtree(&capture.key);
        }
    }
}

pub(super) fn reconcile(
    editor: &mut Editor,
    popups: &[crate::view::scene::ScenePopup],
    capture: &mut Option<Capture>,
) {
    if capture.as_ref().is_some_and(|held| {
        !popups.iter().any(|popup| {
            popup.instance == held.instance && popup.source_anchor.as_ref() == Some(&held.owner)
        })
    }) {
        retire(editor, capture);
    }
}

pub(super) struct Request {
    pub instance: u64,
    pub version: u64,
    mouse: MouseInput,
    column: i32,
    row: i32,
}

impl Request {
    pub fn parse(payload: &[u8]) -> Result<Self, WireError> {
        let (mut instance, mut version, mut mouse, mut column, mut row) = (0, 0, None, 0, 0);
        let mut seen = 0u8;
        for field in Reader::new(payload) {
            let (tag, value) = field?;
            if !(1..=5).contains(&tag) {
                continue;
            }
            let bit = 1 << tag;
            if seen & bit != 0 {
                return Err(WireError::InvalidField("duplicate popup pointer field"));
            }
            seen |= bit;
            match (tag, value) {
                (1, Field::Varint(n)) => instance = n,
                (2, Field::Varint(n)) => version = n,
                (3, Field::Bytes(bytes)) => mouse = super::scene_bridge::mouse(bytes)?,
                (4 | 5, Field::Varint(n)) => {
                    let n = u32::try_from(n)
                        .map_err(|_| WireError::InvalidField("popup pointer coordinate"))?;
                    let coordinate = (n >> 1) as i32 ^ -((n & 1) as i32);
                    if tag == 4 {
                        column = coordinate;
                    } else {
                        row = coordinate;
                    }
                }
                _ => return Err(WireError::InvalidField("popup pointer wire type")),
            }
        }
        if instance == 0 || version == 0 || mouse.is_none() {
            return Err(WireError::InvalidField("popup pointer target"));
        }
        Ok(Self {
            instance,
            version,
            mouse: mouse.unwrap(),
            column,
            row,
        })
    }

    pub fn apply(&self, editor: &mut Editor, capture: &mut Option<Capture>) -> bool {
        if !editor.native_compose_pointer_allowed() {
            retire(editor, capture);
            return false;
        }
        let popups = editor.popups_view();
        reconcile(editor, &popups, capture);
        let Some((index, popup)) = popups.iter().enumerate().find(|(_, popup)| {
            popup.instance == self.instance
                && popup.version == self.version
                && popup.source_anchor.is_some()
                && popup.grid.is_some()
        }) else {
            retire(editor, capture);
            return false;
        };
        // Existing popup facts resolve against the foreground popup. Never
        // let input aimed at a covered lifetime confirm a different stack top.
        if index + 1 != popups.len() {
            retire(editor, capture);
            return false;
        }
        let inside = self.column >= 0
            && self.row >= 0
            && self.column < i32::from(popup.rect.w)
            && self.row < i32::from(popup.rect.h);
        let held = capture
            .as_ref()
            .is_some_and(|held| held.instance == self.instance);
        match self.mouse.kind {
            MouseEventKind::Down(_) if inside => {
                retire(editor, capture);
                *capture = Some(Capture {
                    instance: self.instance,
                    key: crate::view::shell::popup::popup_key(index),
                    owner: popup.source_anchor.clone().expect("validated source owner"),
                });
            }
            MouseEventKind::Down(_) => return false,
            MouseEventKind::Drag(_) | MouseEventKind::Up(_) if !held => return false,
            // A hover leaving the moved box must clear retained hover without
            // running the legacy document/mouse trackers underneath it.
            MouseEventKind::Moved => {}
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
                if !inside =>
            {
                return false
            }
            _ => {}
        }
        let column =
            (i64::from(popup.rect.x) + i64::from(self.column)).clamp(0, i64::from(u16::MAX)) as u16;
        let row =
            (i64::from(popup.rect.y) + i64::from(self.row)).clamp(0, i64::from(u16::MAX)) as u16;
        let event = MouseEvent {
            kind: self.mouse.kind,
            column,
            row,
            modifiers: self.mouse.modifiers,
        };
        if editor.maybe_dismiss_wave_animation(&fresh_input_parser::Event::Mouse(event)) {
            retire(editor, capture);
            return true;
        }
        let repeats = if matches!(
            self.mouse.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            self.mouse.n
        } else {
            1
        };
        for _ in 0..repeats {
            if let Some(mut input) = crate::view::shell::input::mouse(
                event,
                self.mouse.count.clamp(1, 3) as u8,
                editor.config().editor.mouse_wheel_scroll_lines.max(1) as i32,
                16,
            ) {
                let point = fresh_ui::Point::new(
                    (i64::from(popup.rect.x) + i64::from(self.column))
                        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                    (i64::from(popup.rect.y) + i64::from(self.row))
                        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                );
                match &mut input {
                    fresh_ui::Input::Press { pos, .. }
                    | fresh_ui::Input::Release { pos, .. }
                    | fresh_ui::Input::Move { pos, .. }
                    | fresh_ui::Input::Wheel { pos, .. } => *pos = point,
                    fresh_ui::Input::Key(_) => {
                        unreachable!("mouse conversion cannot produce a key")
                    }
                }
                editor.shell_dispatch_popup(input, index);
            }
        }
        if matches!(self.mouse.kind, MouseEventKind::Up(_)) {
            retire(editor, capture);
        }
        true
    }
}
