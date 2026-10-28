//! 录前设备选项的后台元数据刷新；不启动采音，不改变已保存的端点选择。
use crate::audio_devices::{enumerate_active, EndpointInfo, EndpointKind, EndpointSelection};
use std::sync::{Arc, Mutex};

struct Catalog {
    outputs: Result<Vec<EndpointInfo>, String>,
    inputs: Result<Vec<EndpointInfo>, String>,
}
impl Catalog {
    fn failed(message: String) -> Self {
        Self {
            outputs: Err(message.clone()),
            inputs: Err(message),
        }
    }
    fn entries(&self, kind: EndpointKind) -> &Result<Vec<EndpointInfo>, String> {
        match kind {
            EndpointKind::Render => &self.outputs,
            EndpointKind::Capture => &self.inputs,
        }
    }
}

#[derive(Default)]
pub(super) struct DeviceChoices {
    last: Option<Catalog>,
    pending: Option<Arc<Mutex<Option<Catalog>>>>,
}
impl DeviceChoices {
    pub(super) fn refresh(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let result = Arc::new(Mutex::new(None));
        let shared = result.clone();
        let spawned = std::thread::Builder::new()
            .name("rustshot-recording-devices".into())
            .spawn(move || {
                let catalog = std::panic::catch_unwind(|| Catalog {
                    outputs: enumerate_active(EndpointKind::Render)
                        .map_err(|error| error.to_string()),
                    inputs: enumerate_active(EndpointKind::Capture)
                        .map_err(|error| error.to_string()),
                })
                .unwrap_or_else(|_| {
                    Catalog::failed(
                        tr!(
                            "读取音频设备的线程异常退出",
                            "Audio device query stopped unexpectedly"
                        )
                        .into(),
                    )
                });
                *shared.lock().unwrap_or_else(|error| error.into_inner()) = Some(catalog);
            });
        match spawned {
            Ok(_) => self.pending = Some(result),
            Err(error) => self.last = Some(Catalog::failed(error.to_string())),
        }
    }

    pub(super) fn poll(&mut self) -> bool {
        let ready = self.pending.as_ref().and_then(|result| {
            result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        });
        if let Some(catalog) = ready {
            self.last = Some(catalog);
            self.pending = None;
            true
        } else {
            false
        }
    }

    pub(super) fn loading(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn entries(&self, kind: EndpointKind) -> &[EndpointInfo] {
        self.last
            .as_ref()
            .and_then(|catalog| catalog.entries(kind).as_ref().ok())
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn error(&self, kind: EndpointKind) -> Option<&str> {
        self.last
            .as_ref()
            .and_then(|catalog| catalog.entries(kind).as_ref().err())
            .map(String::as_str)
    }

    pub(super) fn saved_missing(&self, kind: EndpointKind, selection: &EndpointSelection) -> bool {
        let EndpointSelection::Fixed { id } = selection else {
            return false;
        };
        self.last
            .as_ref()
            .and_then(|catalog| catalog.entries(kind).as_ref().ok())
            .is_some_and(|entries| !entries.iter().any(|entry| &entry.id == id))
    }

    pub(super) fn selection_label(
        &self,
        kind: EndpointKind,
        selection: &EndpointSelection,
    ) -> String {
        match selection {
            EndpointSelection::Default => tr!("跟随系统默认", "System default").into(),
            EndpointSelection::Fixed { id } => {
                if let Some(entry) = self.entries(kind).iter().find(|entry| &entry.id == id) {
                    option_label(entry, self.entries(kind))
                } else if self.loading() {
                    tr!("正在读取设备…", "Loading devices…").into()
                } else if self.error(kind).is_some() {
                    tr!("未能读取设备列表", "Device list unavailable").into()
                } else if self.saved_missing(kind, selection) {
                    tr!("所选设备暂不可用", "Selected device unavailable").into()
                } else {
                    tr!("已保存的设备", "Saved device").into()
                }
            }
        }
    }
}

pub(super) fn option_label(entry: &EndpointInfo, entries: &[EndpointInfo]) -> String {
    let mut name = entry.name.clone();
    if entries
        .iter()
        .filter(|other| other.name == entry.name)
        .count()
        > 1
    {
        let suffix = endpoint_suffix(&entry.id);
        let mut collisions: Vec<&str> = entries
            .iter()
            .filter(|other| other.name == entry.name && endpoint_suffix(&other.id) == suffix)
            .map(|other| other.id.as_str())
            .collect();
        collisions.push(&entry.id);
        collisions.sort_unstable();
        collisions.dedup();
        // 同名端点仍用各自不透明 ID 区分；前置短 ID 避免长名称裁切后无法识别。
        name = if collisions.len() > 1 {
            // 仅碰撞组加序号；按完整 ID 排序，不受目录顺序、重复项或默认标记影响。
            let ordinal = collisions.binary_search(&entry.id.as_str()).unwrap() + 1;
            format!("[{suffix}·{ordinal}] {name}")
        } else {
            format!("[{suffix}] {name}")
        };
    }
    if entry.is_default {
        trf!("{name}（当前默认）", "{name} (current default)")
    } else {
        name
    }
}

fn endpoint_suffix(id: &str) -> &str {
    let start = id.char_indices().rev().nth(7).map_or(0, |(at, _)| at);
    &id[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_device_choices_keep_fixed_selection_and_distinguish_duplicate_names() {
        let entries = vec![
            EndpointInfo {
                kind: EndpointKind::Render,
                id: "device-A".into(),
                name: "Speaker".into(),
                is_default: true,
            },
            EndpointInfo {
                kind: EndpointKind::Render,
                id: "device-B".into(),
                name: "Speaker".into(),
                is_default: false,
            },
        ];
        assert_ne!(
            option_label(&entries[0], &entries),
            option_label(&entries[1], &entries)
        );
        let choices = DeviceChoices {
            last: Some(Catalog {
                outputs: Ok(entries),
                inputs: Ok(vec![]),
            }),
            pending: None,
        };
        let fixed = EndpointSelection::Fixed { id: "gone".into() };
        assert!(choices.saved_missing(EndpointKind::Render, &fixed));
        assert!(!choices.saved_missing(EndpointKind::Render, &EndpointSelection::Default));
        assert_eq!(fixed, EndpointSelection::Fixed { id: "gone".into() });
        let failed = DeviceChoices {
            last: Some(Catalog::failed("query failed".into())),
            pending: None,
        };
        assert!(!failed.saved_missing(EndpointKind::Render, &fixed));
        assert!(failed.error(EndpointKind::Render).is_some());
    }

    #[test]
    fn recording_device_choices_colliding_suffixes_have_stable_unique_labels() {
        let first = EndpointInfo {
            kind: EndpointKind::Render,
            id: "a-long-opaque-endpoint-12345678".into(),
            name: "Speaker".into(),
            is_default: false,
        };
        let second = EndpointInfo {
            id: "z-long-opaque-endpoint-12345678".into(),
            ..first.clone()
        };
        let entries = vec![second.clone(), first.clone(), first.clone()];
        let expected = ["[12345678·1] Speaker", "[12345678·2] Speaker"];
        let mut reordered = entries.clone();
        reordered.reverse();
        reordered.push(EndpointInfo {
            name: "Other".into(),
            ..first.clone()
        });
        let choices = DeviceChoices {
            last: Some(Catalog {
                outputs: Ok(reordered.clone()),
                inputs: Ok(vec![]),
            }),
            pending: None,
        };
        for (entry, expected) in [first, second].iter().zip(expected) {
            assert_eq!(option_label(entry, &entries), expected);
            assert_eq!(option_label(entry, &reordered), expected);
            let fixed = EndpointSelection::Fixed {
                id: entry.id.clone(),
            };
            assert_eq!(
                choices.selection_label(EndpointKind::Render, &fixed),
                expected
            );
            assert!(!choices.saved_missing(EndpointKind::Render, &fixed));
        }
        reordered[0].is_default = true;
        assert!(option_label(&reordered[0], &reordered).starts_with("[12345678·1] Speaker"));
        assert_eq!(option_label(&reordered[2], &reordered), expected[1]);
    }

    #[test]
    fn recording_device_choices_noncolliding_suffix_labels_keep_their_original_form() {
        let first = EndpointInfo {
            kind: EndpointKind::Capture,
            id: "long-endpoint-12345678".into(),
            name: "Microphone".into(),
            is_default: false,
        };
        let second = EndpointInfo {
            id: "long-endpoint-abcdefgh".into(),
            ..first.clone()
        };
        let entries = [first, second];
        assert_eq!(option_label(&entries[0], &entries), "[12345678] Microphone");
        assert_eq!(option_label(&entries[1], &entries), "[abcdefgh] Microphone");
        assert_eq!(option_label(&entries[0], &entries[..1]), "Microphone");
    }
}
