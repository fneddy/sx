//! Integration tests for `ResultMsg: From<SxResult>`.

use sx_plugin_api::{SxMetaEntry, SxResult};
use sxc::shared::msg::{MetaValue, ResultMsg};

#[test]
fn result_to_msg_text_metadata() {
    let mut res = SxResult::new("calc", 1.0);
    res.metadata
        .push(sx_plugin_api::SxMetaEntry::text("value", "42"));
    let r: ResultMsg = res.into();
    assert_eq!(r.namespace, "calc");
    assert_eq!(r.meta("value"), Some("42"));
    assert!(r.meta_bytes("icon_png").is_none());
}

#[test]
fn result_to_msg_icon_png_stored_as_bytes() {
    let mut res = SxResult::new("apps", 1.0);
    res.title = sx_plugin_api::stabby::option::Option::Some(sx_plugin_api::SxBytes::from_str_bytes("Firefox"));
    res.metadata.push(SxMetaEntry::bytes("icon_png", vec![1, 2, 3]));
    let r: ResultMsg = res.into();
    assert_eq!(r.namespace, "apps");
    assert_eq!(r.title.as_deref(), Some("Firefox"));
    assert_eq!(r.meta_bytes("icon_png"), Some([1u8, 2, 3].as_slice()));
}

#[test]
fn result_to_msg_number_metadata() {
    let mut res = SxResult::new("weather", 1.0);
    res.metadata
        .push(sx_plugin_api::SxMetaEntry::number("temp_c", 18.5));
    let r: ResultMsg = res.into();
    assert_eq!(r.meta_number("temp_c"), Some(18.5));
    // Number entries are not accessible via meta()
    assert!(r.meta("temp_c").is_none());
}

#[test]
fn empty_result_is_empty() {
    let r: ResultMsg = SxResult::new("empty", 0.0).into();
    assert!(r.is_empty(), "all-empty result should be empty");
}

#[test]
fn result_msg_from_file() {
    let r = ResultMsg::from_file("/home/user/doc.txt".to_owned(), 0.75);
    assert_eq!(r.namespace, "file");
    assert_eq!(r.title.as_deref(), Some("/home/user/doc.txt"));
    assert_eq!(r.meta("file_path"), Some("/home/user/doc.txt"));
    assert_eq!(r.score, 0.75);
    assert!(r.meta_bytes("icon_png").is_none());
}

#[test]
fn meta_value_types_are_distinct() {
    let mut res = SxResult::new("test", 1.0);
    res.metadata
        .push(sx_plugin_api::SxMetaEntry::text("k_text", "hello"));
    res.metadata
        .push(sx_plugin_api::SxMetaEntry::number("k_num", 3.14));
    res.metadata
        .push(sx_plugin_api::SxMetaEntry::bytes("k_bytes", vec![0xff]));
    let r: ResultMsg = res.into();
    assert!(matches!(r.metadata.get("k_text").unwrap(), MetaValue::Text(_)));
    assert!(matches!(r.metadata.get("k_num").unwrap(), MetaValue::Number(_)));
    assert!(matches!(r.metadata.get("k_bytes").unwrap(), MetaValue::Bytes(_)));
}
