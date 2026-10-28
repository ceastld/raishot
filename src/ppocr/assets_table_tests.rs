//! 独立表格清单与取消入口；不访问网络或真实缓存。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};

#[test]
fn table_manifest_pins_structure_model_without_text_models() {
    let table = table_manifest().unwrap();
    let tiny = manifest_for(OcrProfile::Tiny).unwrap();
    assert_eq!(table.runtime_api, tiny.runtime_api);
    assert_eq!(table.runtime_manifest, "ppocr-cpu.json");
    assert_ne!(table.cache_revision, tiny.cache_revision);
    assert_ne!(table.models.directory, tiny.models.directory);
    assert_eq!(table.models.structure.name, "table.onnx");
    assert_eq!(table.models.structure.bytes, 7_758_305);
    assert_eq!(
        table.models.structure.sha256,
        "d57a942af6a2f57d6a4a0372573c696a2379bf5857c45e2ac69993f3b334514b"
    );
    assert_eq!(
        table.models.structure.url,
        "https://www.modelscope.cn/models/RapidAI/RapidTable/resolve/v2.0.0/slanet-plus.onnx"
    );
    assert!(same_asset(&table.models.license, &tiny.models.license));
}

#[test]
fn cancelled_table_preparation_stops_before_any_dependency_access() {
    let owner = CancelOwner::new();
    let token = owner.token();
    owner.cancel().unwrap();
    let error = ensure_table_cancellable(&token).unwrap_err();
    assert!(is_cancelled(error.as_ref()));
}
