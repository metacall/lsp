//! Property tests for the encoding mapping.

use meta_call_lsp::position::{Encoding, LineIndex, SourceFile};
use proptest::prelude::*;

fn random_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..200).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #[test]
    fn round_trips_at_char_boundaries(text in random_text(), byte in any::<usize>()) {
        let source = SourceFile::new(text.clone());
        let mut byte = byte % (text.len() + 1);
        while !text.is_char_boundary(byte) {
            byte -= 1;
        }
        for encoding in [Encoding::Utf8, Encoding::Utf16] {
            let position = LineIndex::new(&text).to_position(&text, byte, encoding);
            prop_assert_eq!(
                source.to_byte_offset(position, encoding),
                Some(byte),
                "encoding {:?} byte {}",
                encoding,
                byte
            );
        }
    }

    #[test]
    fn conversions_stay_on_char_boundaries(text in random_text(), byte in any::<usize>()) {
        let byte = byte.min(text.len());
        let index = LineIndex::new(&text);
        let position = index.to_position(&text, byte, Encoding::Utf16);
        let round_trip = index.to_byte_offset(&text, position, Encoding::Utf16);
        prop_assert!(round_trip.is_some_and(|offset| text.is_char_boundary(offset)));
    }
}
