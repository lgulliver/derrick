use std::fs;
use std::path::{Path, PathBuf};

use derrick_caveman::{compress, CompressOutput, Compressor, Intensity};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};

#[test]
fn intensity_serde_round_trip_serializes_lowercase() -> Result<(), TestSerdeError> {
    let cases = [
        (Intensity::Lite, "lite"),
        (Intensity::Full, "full"),
        (Intensity::Ultra, "ultra"),
    ];

    for (intensity, expected) in cases {
        let encoded = intensity.serialize(StringSerializer)?;
        assert_eq!(encoded, expected);
        let decoded = Intensity::deserialize(expected.into_deserializer())?;
        assert_eq!(decoded, intensity);
    }

    Ok(())
}

#[test]
fn compress_empty_input_round_trips_empty() {
    let output = compress("", Intensity::Full);
    assert_eq!(output.text, "");
    assert_eq!(output.stats.chars_in, 0);
    assert_eq!(output.stats.chars_out, 0);
    assert_eq!(output.stats.savings_pct(), 0.0);
}

#[test]
fn stats_chars_words_paragraphs_count_sample() {
    let output = compress(
        "Sure, this is a really small test.\n\nAnother paragraph.",
        Intensity::Full,
    );
    assert_eq!(output.stats.chars_in, 54);
    assert_eq!(output.stats.words_in, 9);
    assert_eq!(output.stats.paragraphs_processed, 2);
    assert_eq!(output.text, "this is small test.\n\nAnother paragraph.");
    assert_eq!(output.stats.words_out, 6);
}

#[test]
fn savings_pct_on_known_input_reports_removed_chars() {
    let output = compress("Sure, this is a really extensive fix.", Intensity::Full);
    assert_eq!(output.text, "this is big fix.");
    assert!((output.stats.savings_pct() - 56.756_756_756_756_76).abs() < f64::EPSILON);
}

#[test]
fn protected_span_count_is_accurate() {
    let output = compress(
        "Review `write_str` and https://example.com/docs before src/lib.rs:42:7",
        Intensity::Ultra,
    );
    assert_eq!(output.stats.preserved_spans, 3);
    assert!(output.text.contains("`write_str`"));
    assert!(output.text.contains("https://example.com/docs"));
    assert!(output.text.contains("src/lib.rs:42:7"));
}

#[test]
fn streaming_state_survives_split_inline_code() {
    let mut compressor = Compressor::new(Intensity::Full);
    let first = compressor.write_str("This is `write");
    let second = compressor.write_str("_str` and a really small test.");
    let final_output = compressor.finish();
    assert!(first.is_empty());
    assert!(second.is_empty());
    assert_eq!(final_output.text, "This is `write_str` and small test.");
    assert_eq!(final_output.stats.preserved_spans, 1);
}

#[test]
fn corpus_cases_match_expected_outputs() -> Result<(), Box<dyn std::error::Error>> {
    for (intensity, dir) in intensity_dirs() {
        for input_path in corpus_inputs(dir)? {
            let input = fs::read_to_string(&input_path)?;
            let expected = fs::read_to_string(input_path.with_extension("out"))?;
            let output = compress(&input, intensity);
            assert_eq!(output.text, expected, "case {}", input_path.display());
        }
    }

    Ok(())
}

#[test]
fn streaming_matches_one_shot_for_corpus() -> Result<(), Box<dyn std::error::Error>> {
    for (intensity, dir) in intensity_dirs() {
        for input_path in corpus_inputs(dir)? {
            let input = fs::read_to_string(&input_path)?;
            let one_shot = compress(&input, intensity);
            for chunk_size in [1, 16, 1024] {
                let streamed = stream_text(&input, intensity, chunk_size);
                assert_eq!(
                    streamed.text,
                    one_shot.text,
                    "case {} chunk {}",
                    input_path.display(),
                    chunk_size
                );
                assert_eq!(streamed.stats.chars_in, one_shot.stats.chars_in);
                assert_eq!(streamed.stats.chars_out, one_shot.stats.chars_out);
            }
        }
    }

    Ok(())
}

fn stream_text(input: &str, intensity: Intensity, chunk_size: usize) -> CompressOutput {
    let mut compressor = Compressor::new(intensity);
    let mut text = String::new();
    let mut start = 0;

    while start < input.len() {
        let end = next_chunk_end(input, start, chunk_size);
        for chunk in compressor.write_str(&input[start..end]) {
            text.push_str(&chunk);
        }
        start = end;
    }

    let mut output = compressor.finish();
    text.push_str(&output.text);
    output.text = text;
    output
}

fn next_chunk_end(input: &str, start: usize, chunk_size: usize) -> usize {
    let mut end = input.len().min(start.saturating_add(chunk_size));
    while end < input.len() && !input.is_char_boundary(end) {
        end = end.saturating_add(1);
    }
    end
}

fn intensity_dirs() -> [(Intensity, &'static str); 3] {
    [
        (Intensity::Lite, "lite"),
        (Intensity::Full, "full"),
        (Intensity::Ultra, "ultra"),
    ]
}

fn corpus_inputs(dir: &str) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join(dir);
    let mut inputs = Vec::new();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "in") {
            inputs.push(path);
        }
    }

    inputs.sort();
    Ok(inputs)
}

#[derive(Debug)]
struct TestSerdeError(String);

impl std::fmt::Display for TestSerdeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TestSerdeError {}

impl serde::ser::Error for TestSerdeError {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

impl serde::de::Error for TestSerdeError {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

struct StringSerializer;

impl serde::Serializer for StringSerializer {
    type Ok = String;
    type Error = TestSerdeError;
    type SerializeSeq = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeTuple = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeTupleStruct = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeTupleVariant = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeMap = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeStruct = serde::ser::Impossible<String, TestSerdeError>;
    type SerializeStructVariant = serde::ser::Impossible<String, TestSerdeError>;

    fn serialize_str(self, value: &str) -> Result<Self::Ok, Self::Error> {
        Ok(value.to_owned())
    }

    fn serialize_bool(self, _value: bool) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected bool".to_owned()))
    }

    fn serialize_i8(self, _value: i8) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected i8".to_owned()))
    }

    fn serialize_i16(self, _value: i16) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected i16".to_owned()))
    }

    fn serialize_i32(self, _value: i32) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected i32".to_owned()))
    }

    fn serialize_i64(self, _value: i64) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected i64".to_owned()))
    }

    fn serialize_u8(self, _value: u8) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected u8".to_owned()))
    }

    fn serialize_u16(self, _value: u16) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected u16".to_owned()))
    }

    fn serialize_u32(self, _value: u32) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected u32".to_owned()))
    }

    fn serialize_u64(self, _value: u64) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected u64".to_owned()))
    }

    fn serialize_f32(self, _value: f32) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected f32".to_owned()))
    }

    fn serialize_f64(self, _value: f64) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected f64".to_owned()))
    }

    fn serialize_char(self, _value: char) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected char".to_owned()))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected bytes".to_owned()))
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected none".to_owned()))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _value: &T) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected some".to_owned()))
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected unit".to_owned()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected unit struct".to_owned()))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Ok(variant.to_owned())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected newtype struct".to_owned()))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(TestSerdeError("unexpected newtype variant".to_owned()))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(TestSerdeError("unexpected seq".to_owned()))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(TestSerdeError("unexpected tuple".to_owned()))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(TestSerdeError("unexpected tuple struct".to_owned()))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(TestSerdeError("unexpected tuple variant".to_owned()))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(TestSerdeError("unexpected map".to_owned()))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Err(TestSerdeError("unexpected struct".to_owned()))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(TestSerdeError("unexpected struct variant".to_owned()))
    }
}
