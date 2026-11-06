//! A frontend may draw one layer at native coordinates. Its old cell footprint
//! must not intercept input routed to the remaining retained surfaces.
use fresh_ui::{
    col, gesture, layer, stack, text, Anchor, Dismiss, Event, GestureKind, Input, Key, Modality,
    Mods, MouseButton, Node, Place, Point, Size, Sizing, Ui,
};
use std::rc::Rc;

fn press() -> Input {
    Input::press(Point::new(3, 2), MouseButton::Left, Mods::NONE)
}
fn surface(message: &'static str) -> Node<&'static str> {
    gesture(col().w(Sizing::Cells(12)).h(Sizing::Cells(5))).on(
        GestureKind::Press,
        Rc::new(move |e: &Event| {
            e.stop();
            Some(message)
        }),
    )
}
fn frame(modality: Modality) -> Node<&'static str> {
    stack().children([
        surface("document"),
        layer()
            .key("popup")
            .anchor(Anchor::Point(1, 1))
            .place(Place::Over)
            .modality(modality)
            .child(surface("popup")),
    ])
}

#[test]
fn native_layer_old_footprint_is_transparent_only_for_the_scoped_pointer() {
    let mut ui = Ui::new();
    ui.frame(frame(Modality::Keyboard), Size::new(20, 10));
    assert_eq!(ui.dispatch(press()).msgs, ["popup"]);
    assert_eq!(
        ui.dispatch_without_pointer_subtrees(press(), &[Key::from("popup")])
            .msgs,
        ["document"]
    );
    assert_eq!(
        ui.dispatch(press()).msgs,
        ["popup"],
        "the original layout is intact after dispatch"
    );
}

#[test]
fn floating_descendants_of_a_projected_subtree_do_not_keep_old_hit_regions() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document"),
            layer()
                .key("popup")
                .anchor(Anchor::Point(0, 0))
                .place(Place::Over)
                .child(
                    stack().children([
                        surface("popup"),
                        layer()
                            .anchor(Anchor::Point(2, 1))
                            .place(Place::Over)
                            .child(surface("nested")),
                    ]),
                ),
        ]),
        Size::new(20, 10),
    );
    assert_eq!(ui.dispatch(press()).msgs, ["nested"]);
    assert_eq!(
        ui.dispatch_without_pointer_subtrees(press(), &[Key::from("popup")])
            .msgs,
        ["document"]
    );
}

#[test]
fn an_excluded_pointer_modal_still_blocks_the_document() {
    let mut ui = Ui::new();
    ui.frame(frame(Modality::Pointer), Size::new(20, 10));
    let result = ui.dispatch_without_pointer_subtrees(press(), &[Key::from("popup")]);
    assert!(result.claimed);
    assert!(result.msgs.is_empty());
}

#[test]
fn transient_dismissal_observes_the_projected_region_as_outside() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document"),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .dismiss(Dismiss::OUTSIDE_POINTER)
                .on_dismiss_handler(Rc::new(|_: &Event| Some("dismiss")))
                .child(surface("popup")),
        ]),
        Size::new(20, 10),
    );
    let result = ui.dispatch_without_pointer_subtrees(press(), &[Key::from("popup")]);
    assert_eq!(result.msgs, ["dismiss", "document"]);
}

#[test]
fn missing_keys_do_not_hide_unrelated_surfaces() {
    let mut ui = Ui::new();
    ui.frame(frame(Modality::Keyboard), Size::new(20, 10));
    assert_eq!(
        ui.dispatch_without_pointer_subtrees(press(), &[Key::from("absent")])
            .msgs,
        ["popup"]
    );
}

#[test]
fn exclusion_is_restored_after_a_panicking_handler() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            gesture(text("doc").w(Sizing::Cells(12)).h(Sizing::Cells(5))).on(
                GestureKind::Press,
                Rc::new(|_: &Event| -> Option<&str> { panic!("probe") }),
            ),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(surface("popup")),
        ]),
        Size::new(20, 10),
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ui.dispatch_without_pointer_subtrees(press(), &[Key::from("popup")]);
    }));
    assert!(result.is_err());
    assert_eq!(ui.dispatch(press()).msgs, ["popup"]);
}

#[test]
fn pointer_exclusion_keeps_the_layers_keyboard_owner() {
    use fresh_ui::{focusable, KeyPress};
    let mut ui = Ui::new();
    ui.frame(
        layer().key("popup").modality(Modality::Keyboard).child(
            focusable(text("popup")).autofocus().on_key(|event| {
                event.stop();
                Some("popup-key")
            }),
        ),
        Size::new(20, 10),
    );
    let result = ui
        .dispatch_without_pointer_subtrees(Input::Key(KeyPress::char('x')), &[Key::from("popup")]);
    assert!(result.claimed);
    assert_eq!(result.msgs, ["popup-key"]);
}

#[test]
fn old_capture_does_not_steal_move_wheel_or_release_from_the_document() {
    use fresh_ui::Axis;
    let watched = |name| {
        gesture(col().w(Sizing::Cells(12)).h(Sizing::Cells(5)))
            .on(
                GestureKind::Move,
                Rc::new(move |event| {
                    event.stop();
                    Some(name)
                }),
            )
            .on(
                GestureKind::Release,
                Rc::new(move |event| {
                    event.stop();
                    Some(name)
                }),
            )
            .on(
                GestureKind::Wheel,
                Rc::new(move |event| {
                    event.stop();
                    Some(name)
                }),
            )
    };
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            watched("document"),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(watched("popup").on(
                    GestureKind::Press,
                    Rc::new(|event| {
                        event.capture_pointer();
                        Some("capture")
                    }),
                )),
        ]),
        Size::new(20, 10),
    );
    ui.dispatch(press());
    assert!(ui.captured().is_some());
    for input in [
        Input::Move {
            pos: Point::new(3, 2),
            mods: Mods::NONE,
        },
        Input::Wheel {
            pos: Point::new(3, 2),
            delta: 1,
            axis: Axis::Vertical,
            mods: Mods::NONE,
        },
        Input::release(Point::new(3, 2), MouseButton::Left, Mods::NONE),
    ] {
        assert_eq!(
            ui.dispatch_without_pointer_subtrees(input, &[Key::from("popup")])
                .msgs,
            ["document"]
        );
    }
    assert!(ui.captured().is_none());
    assert_eq!(
        ui.dispatch(Input::Move {
            pos: Point::new(3, 2),
            mods: Mods::NONE
        })
        .msgs,
        ["popup"]
    );
}

#[test]
fn an_excluded_scrollbar_grab_neither_scrolls_nor_consumes_release() {
    use fresh_ui::viewport;
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document").on(
                GestureKind::Release,
                Rc::new(|event| {
                    event.stop();
                    Some("document-release")
                }),
            ),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(
                    viewport(col().children((0..40).map(|_| text("row"))))
                        .key("popup-scroll")
                        .w(Sizing::Cells(12))
                        .h(Sizing::Cells(5))
                        .scrollbar()
                        .on_scroll(|_| "popup-scroll"),
                ),
        ]),
        Size::new(20, 10),
    );
    ui.dispatch(Input::press(
        Point::new(12, 2),
        MouseButton::Left,
        Mods::NONE,
    ));
    let viewport = ui.find_by_key(&Key::from("popup-scroll")).unwrap();
    let before = ui.scroll(viewport);
    let result = ui.dispatch_without_pointer_subtrees(
        Input::Move {
            pos: Point::new(3, 4),
            mods: Mods::NONE,
        },
        &[Key::from("popup")],
    );
    assert!(result.msgs.is_empty());
    assert_eq!(ui.scroll(viewport), before);
    assert_eq!(
        ui.dispatch_without_pointer_subtrees(
            Input::release(Point::new(3, 2), MouseButton::Left, Mods::NONE),
            &[Key::from("popup")],
        )
        .msgs,
        ["document-release"]
    );
}

#[test]
fn an_excluded_drag_does_not_offer_a_drop_to_the_document() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document")
                .on(GestureKind::Drop, Rc::new(|_| Some("document-drop")))
                .on(
                    GestureKind::Release,
                    Rc::new(|event| {
                        event.stop();
                        Some("document-release")
                    }),
                ),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(gesture(text("drag")).on(
                    GestureKind::Press,
                    Rc::new(|event| {
                        event.start_drag();
                        None
                    }),
                )),
        ]),
        Size::new(20, 10),
    );
    ui.dispatch(Input::press(
        Point::new(2, 1),
        MouseButton::Left,
        Mods::NONE,
    ));
    assert!(ui.dragging());
    let result = ui.dispatch_without_pointer_subtrees(
        Input::release(Point::new(3, 2), MouseButton::Left, Mods::NONE),
        &[Key::from("popup")],
    );
    assert_eq!(result.msgs, ["document-release"]);
    assert!(!ui.dragging());
}

#[test]
fn translated_pointer_uses_its_named_popup_even_when_a_nonmodal_layer_overlaps() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document"),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(surface("popup")),
            layer()
                .key("other")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(surface("other")),
        ]),
        Size::new(20, 10),
    );
    assert_eq!(ui.dispatch(press()).msgs, ["other"]);
    assert_eq!(
        ui.dispatch_to_pointer_subtree(press(), &Key::from("popup"))
            .msgs,
        ["popup"]
    );
    assert_eq!(ui.dispatch(press()).msgs, ["other"]);
    let missing = ui.dispatch_to_pointer_subtree(press(), &Key::from("missing"));
    assert!(!missing.claimed);
    assert!(missing.msgs.is_empty());
}

#[test]
fn a_higher_pointer_modal_blocks_targeted_popup_input_and_source_mapped_input() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            surface("document"),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(surface("popup")),
            layer()
                .key("modal")
                .anchor(Anchor::Point(15, 7))
                .place(Place::Over)
                .modality(Modality::Pointer)
                .child(surface("modal")),
        ]),
        Size::new(20, 10),
    );
    assert!(ui.pointer_is_modal());
    let result = ui.dispatch_to_pointer_subtree(press(), &Key::from("popup"));
    assert!(result.claimed);
    assert!(result.msgs.is_empty());
}

#[test]
fn captured_text_motion_clamps_to_the_source_at_each_edge_and_keeps_wrapped_bytes() {
    let mut ui: Ui<()> = Ui::new();
    ui.frame(
        col().key("body").w(Sizing::Cells(8)).children([
            text("alpha bravo charlie").wrap().key("first"),
            text("👩🏽‍💻 end").key("last"),
        ]),
        Size::new(8, 12),
    );
    let root = Key::from("body");
    let (first, byte) = ui.text_hit_in_subtree(&root, Point::new(-20, -10)).unwrap();
    assert_eq!(ui.key_of(first), Some(Key::from("first")));
    assert_eq!(byte, 0);
    let (wrapped, byte) = ui.text_hit_in_subtree(&root, Point::new(2, 1)).unwrap();
    assert_eq!(wrapped, first);
    assert_eq!(byte, "alpha br".len());
    let (last, byte) = ui.text_hit_in_subtree(&root, Point::new(100, 100)).unwrap();
    assert_eq!(ui.key_of(last), Some(Key::from("last")));
    assert_eq!(byte, "👩🏽‍💻 end".len());
    assert!(ui
        .text_hit_in_subtree(&Key::from("absent"), Point::new(0, 0))
        .is_none());
}

#[test]
fn targeted_popup_input_does_not_release_another_subtrees_drag() {
    let mut ui = Ui::new();
    ui.frame(
        stack().children([
            layer()
                .key("other")
                .anchor(Anchor::Point(15, 0))
                .place(Place::Over)
                .child(
                    gesture(text("drag"))
                        .on(
                            GestureKind::Press,
                            Rc::new(|event| {
                                event.start_drag();
                                None
                            }),
                        )
                        .on(GestureKind::Release, Rc::new(|_| Some("other-release"))),
                ),
            layer()
                .key("popup")
                .anchor(Anchor::Point(1, 1))
                .place(Place::Over)
                .child(surface("popup").on(
                    GestureKind::Release,
                    Rc::new(|event| {
                        event.stop();
                        Some("popup-release")
                    }),
                )),
        ]),
        Size::new(20, 10),
    );
    ui.dispatch(Input::press(
        Point::new(15, 0),
        MouseButton::Left,
        Mods::NONE,
    ));
    assert!(ui.dragging());
    let result = ui.dispatch_to_pointer_subtree(
        Input::release(Point::new(3, 2), MouseButton::Left, Mods::NONE),
        &Key::from("popup"),
    );
    assert_eq!(result.msgs, ["popup-release"]);
    assert!(!ui.dragging());
}

#[test]
fn retiring_a_surface_cancels_its_capture_drag_and_pending_click_without_a_drop() {
    let mut ui: Ui<&'static str> = Ui::new();
    ui.frame(
        layer()
            .key("popup")
            .anchor(Anchor::Point(1, 1))
            .place(Place::Over)
            .child(
                gesture(text("drag"))
                    .on(
                        GestureKind::Press,
                        Rc::new(|event| {
                            event.start_drag();
                            None
                        }),
                    )
                    .on(GestureKind::Click, Rc::new(|_| Some("click")))
                    .on(GestureKind::Drop, Rc::new(|_| Some("drop"))),
            ),
        Size::new(20, 10),
    );
    ui.dispatch(Input::press(
        Point::new(2, 1),
        MouseButton::Left,
        Mods::NONE,
    ));
    assert!(ui.captured().is_some());
    assert!(ui.dragging());
    ui.cancel_pointer_subtree(&Key::from("unrelated"));
    assert!(ui.captured().is_some());
    ui.cancel_pointer_subtree(&Key::from("popup"));
    assert!(ui.captured().is_none());
    assert!(!ui.dragging());
    assert!(ui.drag_autoscroll().is_none());
    assert!(ui
        .dispatch(Input::release(
            Point::new(2, 1),
            MouseButton::Left,
            Mods::NONE
        ))
        .msgs
        .is_empty());
}

#[test]
fn retiring_the_captor_itself_also_ends_its_drag_timer() {
    let mut ui: Ui<()> = Ui::new();
    ui.frame(
        gesture(text("drag")).key("handle").on(
            GestureKind::Press,
            Rc::new(|event| {
                event.start_drag();
                None
            }),
        ),
        Size::new(20, 10),
    );
    ui.dispatch(Input::press(
        Point::new(1, 0),
        MouseButton::Left,
        Mods::NONE,
    ));
    assert!(ui.captured().is_some());
    assert!(ui.dragging());
    ui.cancel_pointer_subtree(&Key::from("handle"));
    assert!(ui.captured().is_none());
    assert!(!ui.dragging());
    assert!(ui.drag_autoscroll().is_none());
}
