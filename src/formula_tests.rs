use super::*;
use crate::recognition::task::CancelOwner;

fn service() -> Service {
    Service {
        endpoint: "https://api.deepseek.com/v1".into(),
        model: "deepseek-flash".into(),
        target: "auto".into(),
        api_key: "".into(),
    }
}

#[test]
fn formula_preserves_nested_chinese_and_control_sequences() {
    let source = r"\text{已有{中文}}+\frac{中文}{x}+普通";
    assert_eq!(
        normalize_latex(source).unwrap(),
        r"\text{已有{中文}}+\frac{中文}{x}+\text{普通}"
    );
    assert_eq!(normalize_latex(r"\中文{不改}").unwrap(), r"\中文{不改}");
    let normalized = normalize_latex("当 x>0 时，f(x)=x").unwrap();
    assert_eq!(normalize_latex(&normalized).unwrap(), normalized);
}

#[test]
fn formula_strips_only_complete_outer_wrappers() {
    assert_eq!(
        normalize_latex("```latex\n\\[x^{2}+1\\]\n```").unwrap(),
        "x^{2}+1"
    );
    assert_eq!(normalize_latex(r"$$\frac{a}{b}$$").unwrap(), r"\frac{a}{b}");
    for invalid in [
        "```latex\nx=1",
        "$x$+$y$",
        r"\[x\] + \[y\]",
        "```json\n{}\n```",
    ] {
        assert!(normalize_latex(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn formula_rejects_broken_groups_environments_and_non_math_commands() {
    for source in [
        r"x^{2",
        r"x}",
        r"\begin{matrix}x\end{aligned}",
        r"\left(x",
        r"\right)x",
        r"\input{secret}",
        r"\href{https://example.com}{x}",
        r"\newcommand{\a}{x}\a",
        r"\begin{document}x\end{document}",
    ] {
        assert!(normalize_latex(source).is_err(), "{source}");
    }
    assert_eq!(
        normalize_latex(r"\left\{x\right.").unwrap(),
        r"\left\{x\right."
    );
    assert!(normalize_latex(&"{".repeat(MAX_GROUP_DEPTH + 1)).is_err());
}

#[test]
fn formula_refuses_blank_brace_base_without_rearranging_labels() {
    let source = r"a+b+\underbrace{\hspace{6em}}_{n\text{项}}";
    assert!(normalize_latex(source).is_err());
    assert!(normalize_latex(r"\overbrace{{\quad\hspace*{2em}}}^{n}").is_err());
    assert!(normalize_latex(r"\underbrace{a+b}_{n}").is_ok());
    let result = result_from_completion(Completion {
        text: source.into(),
        finish_reason: Some("stop".into()),
        incomplete_stream: false,
    });
    assert_eq!(result.raw_output, source);
    assert_eq!(result.latex, source);
    // API 接收不把单个数学段错误升级成整份文档失败；排版该段时仍明确拒绝。
    assert!(result.validation_error.is_none());
    assert!(normalize_latex(&result.latex).is_err());
}

#[test]
fn formula_mixed_markdown_keeps_user_example_and_local_math_diagnostics() {
    let user_example = r"其中 \(n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}\)";
    let source = format!(
        "{user_example}\n\n| 项 | 值 |\n| --- | --- |\n| a | 1 |\n\n错误段 \\(x^{{2\\)，其它段 \\(y=1\\)。"
    );
    let result = result_from_completion(Completion {
        text: source.clone(),
        finish_reason: Some("stop".into()),
        incomplete_stream: false,
    });
    assert_eq!(result.raw_output, source);
    assert_eq!(result.latex, source);
    assert!(result.validation_error.is_none());
    assert_eq!(
        normalize_latex(r"n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}").unwrap(),
        r"n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}"
    );
    assert!(normalize_latex("x^{2").is_err());
    assert_eq!(normalize_latex("y=1").unwrap(), "y=1");
}

#[test]
fn formula_document_fences_preserve_math_identity_and_unfinished_source() {
    let source = "其中 \\(x=1\\)\n\n| a | b |\n| --- | --- |\n| 1 | 2 |";
    for language in ["", "md", "markdown", "latex", "tex", "math"] {
        let raw = format!("```{language}\n{source}\n```");
        let result = result_from_completion(Completion {
            text: raw.clone(),
            finish_reason: Some("stop".into()),
            incomplete_stream: false,
        });
        assert_eq!(result.raw_output, raw);
        assert_eq!(result.latex, source);
        assert!(result.validation_error.is_none());
    }
    for raw in [
        r"\[x=1\]",
        r"$x=1$",
        "  其中 \\(x=1\\)  \n",
        "```markdown\n其中 \\(x=1\\)",
        "```markdown\n其中 \\(x=1\\)```",
        "```text\nhello\n```",
        "```markdown\na\n```\n\n```markdown\nb\n```",
    ] {
        assert_eq!(markdown_source(raw), raw);
    }
    assert_eq!(
        markdown_source("```md\r\n其中 \\(x\\)\r\n```"),
        "其中 \\(x\\)"
    );
}

#[test]
fn formula_long_and_empty_replies_keep_source_with_generation_diagnostics() {
    for source in [
        "x".repeat(MAX_LATEX_BYTES + 1),
        "中".repeat(MAX_LATEX_CHARS + 1),
    ] {
        let response = serde_json::to_vec(&serde_json::json!({
            "choices": [{"message": {"content": source}, "finish_reason": "stop"}]
        }))
        .unwrap();
        let result = result_from_completion(parse_response(&response, false).unwrap());
        assert_eq!(result.raw_output, source);
        assert_eq!(result.latex, source);
        assert!(result.validation_error.is_some());
        let event = serde_json::json!({
            "choices": [{"delta": {"content": source}, "finish_reason": "stop"}]
        });
        let stream = format!("data: {event}\n\ndata: [DONE]\n\n");
        let streamed = result_from_completion(parse_response(stream.as_bytes(), true).unwrap());
        assert_eq!(streamed.raw_output, source);
        assert_eq!(streamed.latex, source);
        assert!(streamed.validation_error.is_some());
    }
    for source in ["", " \n\t", "```markdown\n```"] {
        let result = result_from_completion(Completion {
            text: source.into(),
            finish_reason: Some("stop".into()),
            incomplete_stream: false,
        });
        assert_eq!(result.raw_output, source);
        assert!(result.validation_error.is_some());
    }
}

#[test]
fn formula_truncated_reply_keeps_source_for_editing() {
    let result = result_from_completion(Completion {
        text: "x^{".into(),
        finish_reason: Some("length".into()),
        incomplete_stream: false,
    });
    assert_eq!(result.raw_output, "x^{");
    assert_eq!(result.latex, "x^{");
    assert!(result.validation_error.is_some());
}

#[test]
fn formula_stream_ignores_reasoning_and_requires_a_completed_result() {
    let stream = concat!(
        ": keep-alive\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hidden\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"x^2\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"+1\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let result = result_from_completion(parse_response(stream.as_bytes(), true).unwrap());
    assert_eq!(result.latex, "x^2+1");
    assert!(result.validation_error.is_none());
    let partial = b"data: {\"choices\":[{\"delta\":{\"content\":\"x^2\"}}]}\n\n";
    let result = result_from_completion(parse_response(partial, true).unwrap());
    assert_eq!(result.raw_output, "x^2");
    assert!(result.validation_error.is_some());
}

#[test]
fn formula_upload_is_png_white_composited_dark_normalized_and_not_mutated() {
    let owner = CancelOwner::new();
    let image = Image {
        width: 2,
        height: 1,
        bgra: vec![0, 0, 0, 255, 255, 255, 255, 255],
        readback_ms: 0.0,
    };
    let original = image.bgra.clone();
    let encoded = encode_upload(&image, &owner.token()).unwrap();
    assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(image.bgra, original);
    let mut reader = png::Decoder::new(std::io::Cursor::new(encoded))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels).unwrap();
    assert_eq!(&pixels[..frame.buffer_size()], &[0, 0, 0, 255, 255, 255]);
    let transparent = Image {
        width: 1,
        height: 1,
        bgra: vec![0, 0, 0, 0],
        readback_ms: 0.0,
    };
    let bytes = encode_upload(&transparent, &owner.token()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut pixels).unwrap();
    assert_eq!(&pixels[..3], &[255, 255, 255]);
    let dark = Image {
        width: 1,
        height: 1,
        bgra: vec![0, 0, 0, 255],
        readback_ms: 0.0,
    };
    let bytes = encode_upload(&dark, &owner.token()).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut pixels).unwrap();
    assert_eq!(&pixels[..3], &[255, 255, 255]);
}

#[test]
fn formula_request_is_explicit_image_non_thinking_and_has_no_key_in_body() {
    let mut service = service();
    service.api_key = "private-test-key".into();
    let body = request_body(&service, b"PNG");
    assert_eq!(body["thinking"]["type"], "disabled");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["detail"],
        "original"
    );
    assert!(body["messages"][0]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(!body.to_string().contains("private-test-key"));
    service.endpoint = "https://example.test/v1".into();
    let compatible = request_body(&service, b"PNG");
    assert!(compatible.get("thinking").is_none());
    assert_eq!(
        compatible["messages"][0]["content"][1]["image_url"]["detail"],
        "high"
    );
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
}

#[test]
fn formula_pre_cancel_is_typed_and_does_not_validate_or_send() {
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let invalid = Image {
        width: 0,
        height: 0,
        bgra: vec![],
        readback_ms: 0.0,
    };
    let error = recognize(&service(), &invalid, &owner.token()).unwrap_err();
    assert!(crate::recognition::task::is_cancelled(error.as_ref()));
}

#[test]
fn formula_successful_read_checks_cancellation_but_preserves_real_read_failures() {
    struct Reader<'a> {
        owner: &'a CancelOwner,
        fail: bool,
    }
    impl Read for Reader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.owner.cancel().unwrap();
            if self.fail {
                return Err(std::io::Error::other("private-network-detail"));
            }
            buffer[0] = b'x';
            Ok(1)
        }
    }
    let owner = CancelOwner::new();
    let error = read_bounded(
        Reader {
            owner: &owner,
            fail: false,
        },
        &owner.token(),
    )
    .unwrap_err();
    assert!(crate::recognition::task::is_cancelled(error.as_ref()));
    let owner = CancelOwner::new();
    let error = read_bounded(
        Reader {
            owner: &owner,
            fail: true,
        },
        &owner.token(),
    )
    .unwrap_err();
    assert!(!crate::recognition::task::is_cancelled(error.as_ref()));
    assert!(!error.to_string().contains("private-network-detail"));
}

#[test]
fn formula_upload_limits_long_edge_and_response_limits_are_enforced() {
    let owner = CancelOwner::new();
    let image = Image {
        width: 4096,
        height: 1,
        bgra: vec![255; 4096 * 4],
        readback_ms: 0.0,
    };
    let bytes = encode_upload(&image, &owner.token()).unwrap();
    let reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    assert_eq!(reader.info().width, MAX_UPLOAD_SIDE);
    assert_eq!(reader.info().height, 1);
    let source = std::io::repeat(b'x').take(MAX_RESPONSE_BYTES as u64 + 1);
    assert!(read_bounded(source, &owner.token()).is_err());
    let error = parse_response(
        b"data: {\"error\":{\"message\":\"private-api-key\"}}\n\n",
        true,
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("private-api-key"));
}

#[test]
#[ignore = "requires RUSTSHOT_TEST_DEEPSEEK_KEY and makes one paid image request"]
fn formula_real_deepseek_recognizes_and_renders_small_expression() {
    let key = std::env::var("RUSTSHOT_TEST_DEEPSEEK_KEY").expect("set the test-only DeepSeek key");
    let mut service = service();
    service.api_key = key;
    let source = crate::formula_layout::render("x^2+1", false, 40.0).unwrap();
    let owner = CancelOwner::new();
    let result = recognize(&service, &source, &owner.token()).unwrap();
    assert!(
        result.validation_error.is_none(),
        "{:?}",
        result.validation_error
    );
    let simple: String = result
        .latex
        .chars()
        .filter(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}'))
        .collect();
    assert!(
        simple.contains("x^2+1"),
        "recognized formula differs from the generated sample"
    );
    let preview = crate::formula_layout::render(&result.latex, false, 40.0).unwrap();
    if let Ok(path) = std::env::var("RUSTSHOT_TEST_FORMULA_PREVIEW") {
        crate::export::save_png(preview, std::path::Path::new(&path)).unwrap();
    }
}
