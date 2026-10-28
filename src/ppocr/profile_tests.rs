use super::*;

fn dictionary(profile: OcrProfile) -> Vec<String> {
    vec!["字".to_owned(); profile.dict_class_contract().dictionary_entries]
}

#[test]
fn default_and_explicit_profile_ids_do_not_depend_on_paths() {
    assert_eq!(OcrProfile::default(), OcrProfile::Tiny);
    for profile in [OcrProfile::Tiny, OcrProfile::Small] {
        assert_eq!(OcrProfile::parse(profile.as_str()).unwrap(), profile);
        assert_eq!(profile.id(), profile.as_str());
    }
    for invalid in [
        "",
        "auto",
        "Small",
        " small",
        "small/rec.onnx",
        "ppocrv6_dict.txt",
    ] {
        assert!(OcrProfile::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn profile_slots_and_reported_provider_ids_are_distinct() {
    assert_eq!(OcrProfile::Tiny.index(), 0);
    assert_eq!(OcrProfile::Small.index(), 1);
    assert_eq!(OcrProfile::Tiny.provider_id(), "ppocrv6-tiny");
    assert_eq!(OcrProfile::Small.provider_id(), "ppocrv6-small");
}

#[test]
fn fixed_class_contract_preserves_blank_and_appended_space() {
    for (profile, entries, classes) in [
        (OcrProfile::Tiny, 6904, 6906),
        (OcrProfile::Small, 18708, 18710),
    ] {
        let contract = profile.dict_class_contract();
        assert_eq!(contract.dictionary_entries, entries);
        assert_eq!(contract.ctc_classes, classes);
        assert_eq!(contract.blank_index, 0);
        assert_eq!(contract.space_index, classes - 1);
    }
}

#[test]
fn unicode_multiscalar_entries_remain_verbatim() {
    for profile in [OcrProfile::Tiny, OcrProfile::Small] {
        let mut entries = dictionary(profile);
        entries[1] = "e\u{301}".into();
        entries[2] = "🛅".into();
        let saved = entries.clone();
        profile.validate_dictionary(&entries).unwrap();
        assert_eq!(entries, saved);
    }
}

#[test]
fn profiles_reject_each_others_dictionary_lengths() {
    assert!(OcrProfile::Tiny
        .validate_dictionary(&dictionary(OcrProfile::Small))
        .is_err());
    assert!(OcrProfile::Small
        .validate_dictionary(&dictionary(OcrProfile::Tiny))
        .is_err());
}

#[test]
fn parsed_file_must_not_include_the_implicit_blank_or_appended_space() {
    for profile in [OcrProfile::Tiny, OcrProfile::Small] {
        let mut entries = dictionary(profile);
        entries.push(" ".into());
        assert!(profile.validate_dictionary(&entries).is_err());
        entries.pop();
        entries[1].clear();
        assert!(profile.validate_dictionary(&entries).is_err());
        entries.pop();
        assert!(profile.validate_dictionary(&entries).is_err());
    }
}
