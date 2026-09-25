//! Transport/utterance gates: synthetic PCM, no model, GPU, or device.

use anyhow::Result;
use framed_stream::{EndStatus, FramedReader, FramedWriter, UNIT_BYTES, UNIT_SAMPLES};

use crate::hear::stream::{self, Event, PcmFormat};
use crate::hear::{process_pcm16k, Backend, Heard, Observation, Options, HEAR_RATE};

const F32: &str = "audio/x-pcm;format=f32le;rate=16000;channels=1";
const S16: &str = "audio/x-pcm;format=s16le;rate=16000;channels=1";
// END has a kind, two u64 clocks, a status, and an empty u16-length reason.
const COMPLETE_BYTES: usize = 1 + 8 + 8 + 1 + 2;

#[derive(Default)]
struct Fake {
    clips: Vec<Vec<f32>>,
}

impl Backend for Fake {
    fn hear(&mut self, wave: &[f32], _: &Options) -> Result<Heard> {
        assert!(!wave.is_empty());
        self.clips.push(wave.to_vec());
        Ok(Heard {
            n_tokens: 1,
            hidden: 2,
            rows: vec![0.25, -0.5],
            text: None,
        })
    }
}

fn tone(samples: usize, amp: f32) -> Vec<f32> {
    (0..samples)
        .map(|i| amp * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / HEAR_RATE as f32).sin())
        .collect()
}

fn silence(samples: usize) -> Vec<f32> {
    vec![0.0; samples]
}

fn two_utterances() -> Vec<f32> {
    [
        silence(HEAR_RATE),
        tone(HEAR_RATE * 6 / 5, 0.3),
        silence(HEAR_RATE * 6 / 5),
        tone(HEAR_RATE, 0.2),
        silence(HEAR_RATE * 6 / 5),
    ]
    .concat()
}

fn unfinished_utterance() -> Vec<f32> {
    [silence(HEAR_RATE), tone(HEAR_RATE + 17, 0.3)].concat()
}

fn encode(samples: &[f32], content_type: &str) -> Vec<u8> {
    match content_type {
        F32 => samples.iter().flat_map(|x| x.to_le_bytes()).collect(),
        S16 => samples
            .iter()
            .flat_map(|x| ((*x * 32768.0) as i16).to_le_bytes())
            .collect(),
        _ => panic!("unsupported fixture format"),
    }
}

fn append_pcm(
    writer: &mut FramedWriter<Vec<u8>>,
    samples: &[f32],
    content_type: &str,
    chunk_samples: usize,
) {
    assert!(chunk_samples > 0 && chunk_samples <= HEAR_RATE);
    for chunk in samples.chunks(chunk_samples) {
        writer
            .record(&encode(chunk, content_type), chunk.len() as u64)
            .unwrap();
    }
}

fn pcm_stream(samples: &[f32], content_type: &str, status: EndStatus) -> Vec<u8> {
    let mut writer = FramedWriter::open(Vec::new(), content_type, UNIT_SAMPLES).unwrap();
    // Deliberately split the VAD's 320-sample frames across transport records.
    append_pcm(&mut writer, samples, content_type, 509);
    writer.finish(status).unwrap()
}

fn capture(bytes: &[u8]) -> (Result<()>, Fake, Vec<Event>) {
    let mut backend = Fake::default();
    let mut events = Vec::new();
    let result = FramedReader::open(bytes).and_then(|reader| {
        stream::process(
            reader,
            &mut backend,
            "speaker",
            &Options::default(),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
    });
    (result, backend, events)
}

fn observations(events: &[Event]) -> Vec<&Observation> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Utterance { observation, .. } => Some(observation),
            _ => None,
        })
        .collect()
}

fn baseline(samples: &[f32]) -> (Fake, Vec<Observation>) {
    let mut backend = Fake::default();
    let mut observations = Vec::new();
    process_pcm16k(
        &mut backend,
        samples,
        "speaker",
        &Options::default(),
        &mut |observation| {
            observations.push(observation);
            Ok(())
        },
    )
    .unwrap();
    (backend, observations)
}

#[test]
fn f32le_and_s16le_records_preserve_two_utterances_and_exact_decoded_samples() {
    let wave = two_utterances();
    for format in [F32, S16] {
        let decoded: Vec<f32> = wave
            .iter()
            .map(|x| {
                if format == S16 {
                    ((*x * 32768.0) as i16) as f32 / 32768.0
                } else {
                    *x
                }
            })
            .collect();
        let (expected_backend, expected) = baseline(&decoded);
        let wire = pcm_stream(&wave, format, EndStatus::Complete);
        let (result, backend, events) = capture(&wire);
        result.unwrap();
        assert_eq!(backend.clips, expected_backend.clips, "{format}");
        assert_eq!(backend.clips.len(), 2);
        let actual = observations(&events);
        assert_eq!(actual.len(), 2);
        for (actual, expected) in actual.iter().zip(&expected) {
            assert!(actual.kept());
            assert_eq!(actual.source, "speaker");
            assert_eq!(actual.start_s, expected.start_s);
            assert_eq!(actual.end_s, expected.end_s);
        }
        assert!(actual[1].start_s > actual[0].end_s);
        assert_eq!(events.len(), 3);
        assert!(matches!(events.last(), Some(Event::Complete)));
    }
}

#[test]
fn complete_flushes_the_speech_tail_including_pending_samples_once() {
    let wave = unfinished_utterance();
    let (result, backend, events) = capture(&pcm_stream(&wave, F32, EndStatus::Complete));
    result.unwrap();
    assert_eq!(backend.clips.len(), 1);
    let actual = observations(&events);
    assert_eq!(actual.len(), 1);
    assert!((actual[0].end_s - wave.len() as f64 / HEAR_RATE as f64).abs() < 1e-10);
    assert_eq!(backend.clips[0].last(), wave.last());
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], Event::Utterance { .. }));
    assert!(matches!(events[1], Event::Complete));
}

#[test]
fn speech_at_stream_start_is_not_learned_as_background_noise() {
    let speech = tone(HEAR_RATE * 6 / 5, 0.3);
    for wave in [
        speech.clone(),
        [speech.clone(), silence(HEAR_RATE)].concat(),
    ] {
        let (result, backend, events) = capture(&pcm_stream(&wave, F32, EndStatus::Complete));
        result.unwrap();
        assert_eq!(backend.clips.len(), 1);
        assert_eq!(&backend.clips[0][..speech.len()], speech.as_slice());
        assert_eq!(observations(&events)[0].start_s, 0.0);
        assert!(matches!(events.last(), Some(Event::Complete)));
    }
    let (result, backend, events) = capture(&pcm_stream(
        &speech,
        F32,
        EndStatus::Aborted("disconnected".into()),
    ));
    assert!(result.is_err());
    assert!(backend.clips.is_empty());
    assert!(events.is_empty());
}

#[test]
fn abort_and_truncation_never_flush_partial_speech_or_claim_complete() {
    let wave = unfinished_utterance();
    let complete = pcm_stream(&wave, F32, EndStatus::Complete);
    let aborted = pcm_stream(&wave, F32, EndStatus::Aborted("capture lost".into()));
    let no_end = &complete[..complete.len() - COMPLETE_BYTES];
    let partial_payload = &no_end[..no_end.len() - 1];
    let partial_end = &complete[..complete.len() - 1];
    for wire in [aborted.as_slice(), no_end, partial_payload, partial_end] {
        let (result, backend, events) = capture(wire);
        assert!(result.is_err());
        assert!(
            backend.clips.is_empty(),
            "unfinished audio must not reach inference"
        );
        assert!(
            events.is_empty(),
            "unfinished audio must not be emitted: {events:?}"
        );
    }
    assert!(format!("{:#}", capture(&aborted).0.unwrap_err()).contains("capture lost"));
    assert!(format!("{:#}", capture(no_end).0.unwrap_err()).contains("TRUNCATED"));
}

#[test]
fn gap_discards_partial_speech_and_keeps_odd_pending_samples_in_the_clock() {
    let before = [silence(HEAR_RATE), tone(HEAR_RATE * 4 / 5 + 17, 0.7)].concat();
    let after = [silence(HEAR_RATE), tone(HEAR_RATE, 0.2), silence(HEAR_RATE)].concat();
    let gap_samples = (HEAR_RATE * 2 + 19) as u64;
    let mut writer = FramedWriter::open(Vec::new(), F32, UNIT_SAMPLES).unwrap();
    append_pcm(&mut writer, &before, F32, 509);
    let gap_index = writer.index();
    writer.gap(gap_samples, "receiver dropped frames").unwrap();
    append_pcm(&mut writer, &after, F32, 733);
    let (result, backend, events) = capture(&writer.finish(EndStatus::Complete).unwrap());
    result.unwrap();
    let (expected_backend, expected) = baseline(&after);
    assert_eq!(
        backend.clips, expected_backend.clips,
        "no audio may be stitched across a gap"
    );
    assert_eq!(backend.clips.len(), 1);
    assert_eq!(events.len(), 3);
    let Event::Gap(gap) = &events[0] else {
        panic!("loss must precede the next utterance")
    };
    assert_eq!(gap.index, gap_index);
    assert_eq!(gap.offset, before.len() as u64);
    assert_eq!(gap.extent, gap_samples);
    assert_eq!(gap.reason, "receiver dropped frames");
    let actual = observations(&events);
    let shift = (before.len() as u64 + gap_samples) as f64 / HEAR_RATE as f64;
    assert!((actual[0].start_s - expected[0].start_s - shift).abs() < 1e-10);
    assert!((actual[0].end_s - expected[0].end_s - shift).abs() < 1e-10);
    assert!(matches!(events.last(), Some(Event::Complete)));
}

#[test]
fn malformed_pcm_is_rejected_before_backend_or_output() {
    let cases = [
        (F32, vec![], 0),
        (F32, vec![0; 4], 0),
        (F32, vec![0; 3], 1),
        (F32, vec![0; 4], 2),
        (F32, vec![0; 8], 1),
        (S16, vec![0; 1], 1),
        (S16, vec![0; 4], 1),
        (F32, f32::NAN.to_le_bytes().to_vec(), 1),
        (F32, f32::INFINITY.to_le_bytes().to_vec(), 1),
        (F32, f32::NEG_INFINITY.to_le_bytes().to_vec(), 1),
        (F32, 1.01_f32.to_le_bytes().to_vec(), 1),
        (F32, (-1.01_f32).to_le_bytes().to_vec(), 1),
        (F32, vec![0; (HEAR_RATE + 1) * 4], (HEAR_RATE + 1) as u64),
        (F32, vec![], u64::MAX),
    ];
    for (format, payload, extent) in cases {
        let mut writer = FramedWriter::open(Vec::new(), format, UNIT_SAMPLES).unwrap();
        writer.record(&payload, extent).unwrap();
        let (result, backend, events) = capture(&writer.finish(EndStatus::Complete).unwrap());
        assert!(
            result.is_err(),
            "accepted {format}, {} bytes, extent {extent}",
            payload.len()
        );
        assert!(backend.clips.is_empty());
        assert!(events.is_empty());
    }
}

#[test]
fn invalid_headers_fail_both_preflight_and_processing_before_backend() {
    let cases = [
        (F32, UNIT_BYTES),
        ("audio/L16;rate=16000;channels=1", UNIT_SAMPLES),
        (
            "audio/x-pcm;format=f32le;rate=44100;channels=1",
            UNIT_SAMPLES,
        ),
        ("audio/x-pcm;format=f32le;rate=0;channels=1", UNIT_SAMPLES),
        (
            "audio/x-pcm;format=f32le;rate=16000;channels=2",
            UNIT_SAMPLES,
        ),
        (
            "audio/x-pcm;format=f32be;rate=16000;channels=1",
            UNIT_SAMPLES,
        ),
        ("audio/x-pcm;format=f32le;channels=1", UNIT_SAMPLES),
        ("audio/x-pcm;rate=16000;channels=1", UNIT_SAMPLES),
        ("audio/x-pcm;format=f32le;rate=16000", UNIT_SAMPLES),
        (
            "audio/x-pcm;format=f32le;rate=16000;rate=16000;channels=1",
            UNIT_SAMPLES,
        ),
        (
            "audio/x-pcm;format=f32le;rate=16000;channels=1;extra=x",
            UNIT_SAMPLES,
        ),
        ("audio/x-pcm;format=f32le;rate=16000;channels", UNIT_SAMPLES),
    ];
    for (format, unit) in cases {
        let wire = FramedWriter::open(Vec::new(), format, unit)
            .unwrap()
            .finish(EndStatus::Complete)
            .unwrap();
        assert!(
            stream::open(wire.as_slice()).is_err(),
            "accepted header {format} ({unit})"
        );
        let (result, backend, events) = capture(&wire);
        assert!(result.is_err());
        assert!(backend.clips.is_empty());
        assert!(events.is_empty());
    }
}

#[test]
fn format_changes_abort_pending_speech_but_equivalent_parameters_are_accepted() {
    let wave = unfinished_utterance();
    for changed in [
        S16,
        "audio/x-pcm;format=f32le;rate=24000;channels=1",
        "text/plain",
    ] {
        let mut writer = FramedWriter::open(Vec::new(), F32, UNIT_SAMPLES).unwrap();
        append_pcm(&mut writer, &wave, F32, 509);
        writer.record_as(changed, &[0; 4], 1).unwrap();
        let (result, backend, events) = capture(&writer.finish(EndStatus::Complete).unwrap());
        assert!(result.is_err(), "accepted changed format {changed}");
        assert!(backend.clips.is_empty());
        assert!(events.is_empty());
    }

    let equivalent = "audio/x-pcm; channels = 1; rate = 16000; format = f32le";
    let mut writer = FramedWriter::open(Vec::new(), F32, UNIT_SAMPLES).unwrap();
    for chunk in wave.chunks(509) {
        writer
            .record_as(equivalent, &encode(chunk, F32), chunk.len() as u64)
            .unwrap();
    }
    let (result, backend, events) = capture(&writer.finish(EndStatus::Complete).unwrap());
    result.unwrap();
    assert_eq!(backend.clips.len(), 1);
    assert!(matches!(events.last(), Some(Event::Complete)));
    for rate in [16000, 24000, 48000] {
        for format in ["f32le", "s16le"] {
            assert!(PcmFormat::parse(&format!(
                "audio/x-pcm;format={format};rate={rate};channels=1"
            ))
            .is_ok());
        }
    }
}

#[test]
fn output_failure_stops_before_second_inference_without_retry_or_complete() {
    let wire = pcm_stream(&two_utterances(), F32, EndStatus::Complete);
    let (reader, _) = stream::open(wire.as_slice()).unwrap();
    let mut backend = Fake::default();
    let mut attempts = 0;
    let error = stream::process(
        reader,
        &mut backend,
        "speaker",
        &Options::default(),
        &mut |event| {
            attempts += 1;
            assert!(matches!(event, Event::Utterance { .. }));
            anyhow::bail!("output disconnected")
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("output disconnected"));
    assert_eq!(attempts, 1);
    assert_eq!(backend.clips.len(), 1);
}

#[test]
fn gap_callback_failure_stops_before_later_audio() {
    let mut writer = FramedWriter::open(Vec::new(), F32, UNIT_SAMPLES).unwrap();
    writer.gap(17, "loss").unwrap();
    append_pcm(&mut writer, &two_utterances(), F32, 509);
    let wire = writer.finish(EndStatus::Complete).unwrap();
    let (reader, _) = stream::open(wire.as_slice()).unwrap();
    let mut backend = Fake::default();
    let mut attempts = 0;
    let error = stream::process(
        reader,
        &mut backend,
        "speaker",
        &Options::default(),
        &mut |event| {
            attempts += 1;
            assert!(matches!(event, Event::Gap(_)));
            anyhow::bail!("cannot report loss")
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("cannot report loss"));
    assert_eq!(attempts, 1);
    assert!(backend.clips.is_empty());
}

#[test]
fn hostile_data_and_gap_extents_cannot_wrap_the_absolute_clock() {
    // Build harmless frames, then tamper the documented wire clock fields.
    // The fixture writer itself never performs overflowing arithmetic.
    for second_is_gap in [false, true] {
        let mut writer = FramedWriter::open(Vec::new(), F32, UNIT_SAMPLES).unwrap();
        writer.gap(1, "").unwrap();
        if second_is_gap {
            writer.gap(4, "").unwrap();
        } else {
            writer.record(&[0; 16], 4).unwrap();
        }
        let mut wire = writer.finish(EndStatus::Complete).unwrap();
        let preamble = 8 + 2 + 2 + 2 + F32.len() + 2 + UNIT_SAMPLES.len();
        let first_extent = preamble + 1 + 8 + 8;
        let second_frame = preamble + 1 + 8 + 8 + 8 + 2;
        let second_offset = second_frame + 1 + 8;
        wire[first_extent..first_extent + 8].copy_from_slice(&(u64::MAX - 3).to_le_bytes());
        wire[second_offset..second_offset + 8].copy_from_slice(&(u64::MAX - 3).to_le_bytes());
        let (result, backend, events) = capture(&wire);
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("overflow"), "{error:#}");
        assert!(backend.clips.is_empty());
        assert_eq!(
            events.len(),
            1,
            "the overflowing record must not be delivered"
        );
        assert!(matches!(events[0], Event::Gap(_)));
    }
}
