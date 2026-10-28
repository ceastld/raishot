//! 字幕沿用原元素的布局与排版，仅在同次画面绘制成功后绘制。
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
use std::{cell::Cell, rc::Rc};

pub(super) fn gated_subtitle(element: AnyElement, painted: Rc<Cell<bool>>) -> impl IntoElement {
    SubtitleGate { element, painted }
}

struct SubtitleGate {
    element: AnyElement,
    painted: Rc<Cell<bool>>,
}

impl IntoElement for SubtitleGate {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SubtitleGate {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.element.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.painted.get() {
            self.element.paint(window, cx);
        }
    }
}
