//! 贴图文字的来源与复制发布合同；不拥有识别结果或剪贴板资源。

/// 显式记录正文来源，避免把原生标注文字当作 OCR 结果处理。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TextSource {
    #[default]
    None,
    Ocr,
    Qr,
    Native,
}

#[derive(Clone, Copy)]
pub(super) enum Content {
    Body,
    Selection,
}

/// 借用现有正文发布，保留全部原始字符；只有发布成功才返回 `true`。
///
/// 空白正文不触发发布。失败原样交回调用方，不清除或重建正文，
/// 使调用方可以在同一份识别结果上重试。
pub(super) fn publish_text<E>(
    text: &str,
    content: Content,
    write: impl FnOnce(&str) -> Result<(), E>,
) -> Result<bool, E> {
    let empty = match content {
        Content::Body => text.trim().is_empty(),
        Content::Selection => text.is_empty(),
    };
    if empty {
        return Ok(false);
    }
    write(text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{publish_text, Content};
    use std::{cell::Cell, io};

    #[test]
    fn publish_preserves_unicode_whitespace_and_line_endings() {
        let text = " \t中文🙂\r\nSecond line\n第三行\r尾行\u{3000} ";
        let result = publish_text(text, Content::Body, |payload| {
            assert_eq!(payload.as_bytes(), text.as_bytes());
            assert_eq!(payload.as_ptr(), text.as_ptr());
            Ok::<(), io::Error>(())
        });
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn empty_and_unicode_whitespace_do_not_publish() {
        for text in ["", " \t\r\n", "\u{3000}\u{2003}\u{a0}"] {
            let result = publish_text(text, Content::Body, |_| -> Result<(), io::Error> {
                panic!("空白正文不得调用发布器");
            });
            assert!(matches!(result, Ok(false)));
        }
    }

    #[test]
    fn success_is_reported_after_the_writer_succeeds() {
        let published = Cell::new(false);
        let result = publish_text("已识别的正文", Content::Body, |_| {
            assert!(!published.get());
            published.set(true);
            Ok::<(), io::Error>(())
        });
        assert!(published.get());
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn typed_writer_error_is_returned_without_conversion() {
        let result = publish_text("正文", Content::Body, |_| {
            Err(io::Error::from_raw_os_error(5))
        });
        let error = result.expect_err("发布失败不得被报告为成功");
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn failure_then_retry_borrows_the_same_existing_body() {
        let text = String::from(" 保留的 OCR 正文🙂\r\n第二行 ");
        let body_ptr = text.as_ptr();
        let calls = Cell::new(0);
        let first = publish_text(&text, Content::Body, |payload| {
            calls.set(calls.get() + 1);
            assert_eq!(payload.as_ptr(), body_ptr);
            Err(io::Error::from_raw_os_error(5))
        });
        assert_eq!(first.expect_err("首次发布应失败").raw_os_error(), Some(5));
        assert_eq!(text, " 保留的 OCR 正文🙂\r\n第二行 ");

        let retry = publish_text(&text, Content::Body, |payload| {
            calls.set(calls.get() + 1);
            assert_eq!(payload.as_ptr(), body_ptr);
            assert_eq!(payload, " 保留的 OCR 正文🙂\r\n第二行 ");
            Ok::<(), io::Error>(())
        });
        assert!(matches!(retry, Ok(true)));
        assert_eq!(calls.get(), 2);
        assert_eq!(text.as_ptr(), body_ptr);
        assert_eq!(text, " 保留的 OCR 正文🙂\r\n第二行 ");
    }

    #[test]
    fn selected_whitespace_is_published_verbatim_and_only_empty_ranges_are_skipped() {
        for text in [" ", "\r\n", "\t\u{3000}\u{2003}\u{a0}"] {
            let calls = Cell::new(0);
            let result = publish_text(text, Content::Selection, |payload| {
                calls.set(calls.get() + 1);
                assert_eq!(payload.as_bytes(), text.as_bytes());
                Ok::<(), io::Error>(())
            });
            assert!(matches!(result, Ok(true)));
            assert_eq!(calls.get(), 1);
        }
        let result = publish_text("", Content::Selection, |_| -> Result<(), io::Error> {
            panic!("空选择不得调用发布器");
        });
        assert!(matches!(result, Ok(false)));
    }
}
