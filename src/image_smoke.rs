use crate::{capture::Image, capture_view::CaptureView, Result};
use gpui::{App, Entity, Window};
use std::{cell::RefCell, path::PathBuf, rc::Rc};
pub fn fixture() -> Result<(Image, PathBuf)> {
    Err("图片编辑验收尚未接入".into())
}
pub fn schedule(
    _: &Entity<CaptureView>,
    _: &mut Window,
    cx: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("图片编辑验收尚未接入".into());
    cx.quit();
}
