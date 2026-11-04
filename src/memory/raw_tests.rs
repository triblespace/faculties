//! Exact raw reading of synthetic Memory facts. No live deployment inputs.
use crate::collection_names::open;
use crate::memory::{cli, Memory};
use crate::out::{Out, Part};
use crate::schemas::memory::{ctx, DEFAULT_SCOPE_ID, KIND_CHUNK_ID};
use crate::storage::{initialize_signer, Storage};
use clap::Parser;
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;
use triblespace::prelude::*;

#[test]
fn raw_memory_cli_and_library_preserve_bytes_and_default_framing() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("raw.pile");
    let key = directory.path().join("raw.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    let storage = Storage::new(pile.clone(), Some(key.clone()));
    let memory = Memory::with_storage(storage.clone());
    let cases = ["", "\n\n", " \t", "\nleading\n", "CRLF\r\n",
        "trailing \t\n", "\u{2003}\u{a0}", "e\u{301} 🐝\n"];
    for text in cases {
        let id = *fucid();
        let fragment = entity! {
            ExclusiveId::force_ref(&id) @
            metadata::tag: &KIND_CHUNK_ID,
            ctx::summary: text,
        };
        storage.with_pile(|pile, signer| {
            let collection = open(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
            pile.commit(collection, signer, fragment)?;
            Ok(())
        }).unwrap();
        let call = |raw: bool| {
            let mut argv = vec!["memory".to_owned(), "--pile".into(),
                pile.to_str().unwrap().into(), "--key".into(), key.to_str().unwrap().into()];
            if raw { argv.push("raw".into()); }
            argv.push(format!("{id:x}"));
            let mut parts = Vec::new();
            let code = cli::execute(cli::Cli::try_parse_from(argv).unwrap(),
                &mut Out::new(&mut |part| { parts.push(part); Ok(()) })).unwrap();
            assert_eq!(code, 0);
            parts
        };
        assert!(matches!(call(true).as_slice(), [Part::Text { text: actual }] if actual == text));
        assert!(matches!(call(false).as_slice(), [Part::Text { text: actual }]
            if actual == &format!("{}\n", text.trim_end())));
        let mut parts = Vec::new();
        memory.raw(id, &mut Out::new(&mut |part| { parts.push(part); Ok(()) })).unwrap();
        assert!(matches!(parts.as_slice(), [Part::Text { text: actual }] if actual == text));
    }
}

#[test]
fn raw_memory_multiple_readings_choose_existing_min_handle_policy() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("readings.pile");
    let key = directory.path().join("readings.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    let storage = Storage::new(pile.clone(), Some(key.clone()));
    let id = *fucid();
    let readings = ["\nfirst \t", "second\n\n"];
    let fragment = entity! {
        ExclusiveId::force_ref(&id) @
        metadata::tag: &KIND_CHUNK_ID,
        ctx::summary*: readings,
    };
    let selected = crate::memory_cover::chunk_summary_handle(fragment.facts(), id).unwrap();
    let expected = readings.into_iter().find(|text| {
        let blob: Blob<blobencodings::UTF8String> = (*text).to_owned().to_blob();
        blob.get_handle() == selected
    }).unwrap();
    storage.with_pile(|pile, signer| {
        let collection = open(pile, DEFAULT_SCOPE_ID, signer.verifying_key())?;
        pile.commit(collection, signer, fragment)?;
        Ok(())
    }).unwrap();
    let mut parts = Vec::new();
    Memory::with_storage(storage).raw(id,
        &mut Out::new(&mut |part| { parts.push(part); Ok(()) })).unwrap();
    assert!(matches!(parts.as_slice(), [Part::Text { text }] if text == expected));
}

#[test]
fn raw_memory_requires_full_id_and_text_reading() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("grammar.pile");
    let key = directory.path().join("grammar.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    for args in [vec!["raw"], vec!["raw", "abc"],
        vec!["raw", "00000000000000000000000000000000"],
        vec!["raw", "not-an-id", "extra"]] {
        let mut argv = vec!["memory", "--pile", pile.to_str().unwrap(),
            "--key", key.to_str().unwrap()];
        argv.extend(args);
        let mut output = Vec::new();
        assert!(cli::execute(cli::Cli::try_parse_from(argv).unwrap(),
            &mut Out::new(&mut |part| { output.push(part); Ok(()) })).is_err());
        assert!(output.is_empty());
    }
    let mut output = Vec::new();
    assert!(Memory::new(pile, Some(key)).raw(*fucid(),
        &mut Out::new(&mut |part| { output.push(part); Ok(()) })).is_err());
    assert!(output.is_empty());
}
