//! 局部样式的磁盘字段：字体存名字，颜色按浅色基准原样，段落只存非默认项。
//! 未知字段直接拒读，避免旧版本静默丢弃以后新增的格式。
use crate::{
    annotation::rich::{
        paragraph_count, Baseline, Cell, Paragraph, RichText, Run, RunStyle, Table, MAX_PARAGRAPHS,
    },
    text_format::{Alignment, FontFamily},
    Result,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WireRichText {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    runs: Vec<WireRun>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paragraphs: Vec<WireParagraph>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tables: Vec<WireTable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    links: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    card_fill: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    plain: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRun {
    start: u32,
    end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strike: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireParagraph {
    index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    align: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    indent: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    first_indent: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    space_before: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    space_after: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line_height: Option<f32>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    wrap: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote_bar: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rule_after: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    table_row: Option<[u16; 2]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireTable {
    columns: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    border: Option<u32>,
    #[serde(default, skip_serializing_if = "is_zero")]
    padding: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cells: Vec<WireCell>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireCell {
    row: u16,
    col: u16,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    col_span: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    header: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<u32>,
}

fn is_false(v: &bool) -> bool {
    !*v
}
fn is_true(v: &bool) -> bool {
    *v
}
fn yes() -> bool {
    true
}
fn is_zero(v: &f32) -> bool {
    *v == 0.
}
fn one() -> u16 {
    1
}
fn is_one(v: &u16) -> bool {
    *v == 1
}

fn align_name(align: Alignment) -> &'static str {
    match align {
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
    }
}
fn align_model(name: &str) -> Result<Alignment> {
    Ok(match name {
        "left" => Alignment::Left,
        "center" => Alignment::Center,
        "right" => Alignment::Right,
        _ => return Err("文字段落对齐方式无效".into()),
    })
}
fn font_name_valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}

impl From<&Paragraph> for WireParagraph {
    fn from(p: &Paragraph) -> Self {
        Self {
            index: 0,
            align: p.align.map(|a| align_name(a).into()),
            indent: p.indent,
            first_indent: p.first_indent,
            space_before: p.space_before,
            space_after: p.space_after,
            line_height: p.line_height,
            wrap: p.wrap,
            background: p.background,
            quote_bar: p.quote_bar,
            rule_after: p.rule_after,
            table_row: p.table_row.map(|(t, r)| [t, r]),
        }
    }
}

impl WireRichText {
    pub(crate) fn from_model(rich: &RichText) -> Self {
        Self {
            runs: rich
                .runs
                .iter()
                .map(|run| {
                    let s = run.style;
                    WireRun {
                        start: run.start,
                        end: run.end,
                        font: s.font.map(|f| f.name().into()),
                        size: s.size,
                        color: s.color,
                        bold: s.bold,
                        italic: s.italic,
                        underline: s.underline,
                        strike: s.strike,
                        background: s.background,
                        link: s.link,
                        baseline: match s.baseline {
                            Baseline::Normal => None,
                            Baseline::Sub => Some("sub".into()),
                            Baseline::Super => Some("super".into()),
                        },
                    }
                })
                .collect(),
            paragraphs: rich
                .paragraphs
                .iter()
                .enumerate()
                .filter(|(_, p)| **p != Paragraph::default())
                .map(|(index, p)| WireParagraph {
                    index: index as u32,
                    ..p.into()
                })
                .collect(),
            tables: rich
                .tables
                .iter()
                .map(|t| WireTable {
                    columns: t.columns,
                    border: t.border,
                    padding: t.padding,
                    cells: t
                        .cells
                        .iter()
                        .map(|c| WireCell {
                            row: c.row,
                            col: c.col,
                            col_span: c.col_span,
                            header: c.header,
                            background: c.background,
                        })
                        .collect(),
                })
                .collect(),
            links: rich.links.iter().map(|l| l.to_string()).collect(),
            card_fill: rich.card_fill,
            plain: rich.plain,
        }
    }

    /// `font` 把字体名映射为字体身份：严格验证时是符号，安装时是本机字体。
    pub(crate) fn model(
        &self,
        text: &str,
        font: &mut dyn FnMut(&str) -> FontFamily,
    ) -> Result<RichText> {
        let count = paragraph_count(text);
        if count > MAX_PARAGRAPHS || self.paragraphs.len() > count {
            return Err("文字段落数量超过限制".into());
        }
        let mut paragraphs = vec![Paragraph::default(); count];
        let mut next = 0;
        for p in &self.paragraphs {
            let index = p.index as usize;
            if index < next || index >= count {
                return Err("文字段落序号无效".into());
            }
            next = index + 1;
            paragraphs[index] = Paragraph {
                align: p.align.as_deref().map(align_model).transpose()?,
                indent: p.indent,
                first_indent: p.first_indent,
                space_before: p.space_before,
                space_after: p.space_after,
                line_height: p.line_height,
                wrap: p.wrap,
                background: p.background,
                quote_bar: p.quote_bar,
                rule_after: p.rule_after,
                table_row: p.table_row.map(|[t, r]| (t, r)),
            };
        }
        let runs = self
            .runs
            .iter()
            .map(|r| {
                if r.font.as_deref().is_some_and(|name| !font_name_valid(name)) {
                    return Err("文字样式片段字体名无效".into());
                }
                Ok(Run {
                    start: r.start,
                    end: r.end,
                    style: RunStyle {
                        font: r.font.as_deref().map(&mut *font),
                        size: r.size,
                        color: r.color,
                        bold: r.bold,
                        italic: r.italic,
                        underline: r.underline,
                        strike: r.strike,
                        background: r.background,
                        link: r.link,
                        baseline: match r.baseline.as_deref() {
                            None => Baseline::Normal,
                            Some("sub") => Baseline::Sub,
                            Some("super") => Baseline::Super,
                            Some(_) => return Err("文字样式片段基线无效".into()),
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let rich = RichText {
            runs,
            paragraphs,
            tables: self
                .tables
                .iter()
                .map(|t| Table {
                    columns: t.columns,
                    border: t.border,
                    padding: t.padding,
                    cells: t
                        .cells
                        .iter()
                        .map(|c| Cell {
                            row: c.row,
                            col: c.col,
                            col_span: c.col_span,
                            header: c.header,
                            background: c.background,
                        })
                        .collect(),
                })
                .collect(),
            links: self.links.iter().map(|l| l.as_str().into()).collect(),
            card_fill: self.card_fill,
            plain: self.plain,
        };
        rich.validate(text)?;
        Ok(rich)
    }

    pub(crate) fn validate(&self, text: &str) -> Result<()> {
        self.model(text, &mut |_| FontFamily::default()).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(text: &str) -> RichText {
        let mut rich = RichText::new(text);
        rich.links.push("https://example.com/".into());
        rich.runs = vec![
            Run {
                start: 0,
                end: 6,
                style: RunStyle {
                    size: Some(1.5),
                    bold: Some(true),
                    font: Some(FontFamily::default()),
                    ..Default::default()
                },
            },
            Run {
                start: 7,
                end: 11,
                style: RunStyle {
                    color: Some(0x0645ad),
                    underline: Some(true),
                    link: Some(0),
                    baseline: Baseline::Super,
                    ..Default::default()
                },
            },
        ];
        rich.paragraphs[0].space_after = 0.5;
        rich.paragraphs[2] = Paragraph {
            align: Some(Alignment::Center),
            wrap: false,
            table_row: Some((0, 0)),
            ..Default::default()
        };
        rich.tables.push(Table {
            columns: 2,
            border: Some(0xcccccc),
            padding: 0.3,
            cells: vec![
                Cell {
                    row: 0,
                    col: 0,
                    col_span: 1,
                    header: true,
                    background: Some(0xeeeeee),
                },
                Cell {
                    row: 0,
                    col: 1,
                    col_span: 1,
                    header: true,
                    background: None,
                },
            ],
        });
        rich.card_fill = Some(0x1e1e1e);
        rich.validate(text).unwrap();
        rich
    }

    #[test]
    fn rich_wire_round_trips_and_keeps_default_paragraphs_sparse() {
        let text = "标题 link\n正文\n列一\t列二";
        let rich = sample(text);
        let wire = WireRichText::from_model(&rich);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json["paragraphs"].as_array().unwrap().len(),
            2,
            "默认段落不写"
        );
        assert_eq!(json["runs"][0]["font"], "Microsoft YaHei UI");
        assert_eq!(json["runs"][1]["baseline"], "super");
        assert_eq!(json["paragraphs"][1]["wrap"], false);
        assert!(json["tables"][0]["cells"][0].get("colSpan").is_none());
        let decoded: WireRichText = serde_json::from_value(json).unwrap();
        let restored = decoded.model(text, &mut |_| FontFamily::default()).unwrap();
        assert_eq!(restored, rich);
    }

    #[test]
    fn rich_wire_rejects_unknown_fields_bad_indices_and_names() {
        let text = "标题 link\n正文\n列一\t列二";
        let json = serde_json::to_value(WireRichText::from_model(&sample(text))).unwrap();
        let reject = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut bad = json.clone();
            edit(&mut bad);
            serde_json::from_value::<WireRichText>(bad)
                .map_err(|e| e.to_string())
                .and_then(|w| {
                    w.model(text, &mut |_| FontFamily::default())
                        .map_err(|e| e.to_string())
                })
                .unwrap_err()
        };
        reject(&|v| v["future"] = serde_json::json!(1));
        reject(&|v| v["runs"][0]["shadow"] = serde_json::json!(true));
        reject(&|v| v["paragraphs"][1]["index"] = serde_json::json!(0));
        reject(&|v| v["paragraphs"][1]["index"] = serde_json::json!(9));
        reject(&|v| v["paragraphs"][0]["align"] = serde_json::json!("justify"));
        reject(&|v| v["runs"][0]["font"] = serde_json::json!(""));
        reject(&|v| v["runs"][1]["baseline"] = serde_json::json!("middle"));
        reject(&|v| v["runs"][1]["start"] = serde_json::json!(3));
        reject(&|v| v["links"][0] = serde_json::json!("javascript:x"));
    }

    #[test]
    fn rich_wire_resolves_run_fonts_through_caller() {
        let text = "ab";
        let mut rich = RichText::new(text);
        rich.runs.push(Run {
            start: 0,
            end: 1,
            style: RunStyle {
                font: Some(FontFamily::default()),
                ..Default::default()
            },
        });
        let wire = WireRichText::from_model(&rich);
        let mut seen = Vec::new();
        let model = wire
            .model(text, &mut |name| {
                seen.push(name.to_owned());
                FontFamily(7)
            })
            .unwrap();
        assert_eq!(seen, ["Microsoft YaHei UI"]);
        assert_eq!(model.runs[0].style.font, Some(FontFamily(7)));
    }
}
