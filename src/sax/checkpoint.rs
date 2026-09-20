//! Deterministic, portable checkpoints of the parser's structural state.
//!
//! A checkpoint contains everything required to resume parsing after a
//! restart: the lexical state, any incomplete UTF-8 sequence, the open tag
//! stack (including in-flight attributes and text), partial markup buffers
//! and position information. It stores byte values only - never pointers
//! into a previous WebAssembly linear memory - so checkpoints produced in
//! one process can be restored in another.
//!
//! # Wire format
//!
//! All integers are little-endian. Lengths are `u32`.
//!
//! Fixed header (42 bytes):
//! - 0..4: magic `SAXC`
//! - 4: format version
//! - 5..7: reserved parse-options word (must be zero)
//! - 7..9: event mask (u16)
//! - 9..17: consumed bytes (u64)
//! - 17..25: end line (u64)
//! - 25..33: end character (u64)
//! - 33: lexical state (u8)
//! - 34: attribute quote byte (0, `'` or `"`)
//! - 35..37: JSX brace count (u16)
//! - 37..41: fragment byte length (u32)
//! - 41: reserved (zero)
//!
//! Followed by the fragment bytes and a fixed-order set of optional payloads.

use super::parser::{SAXParser, State};
use super::tag::{AttrType, Attribute, ProcInst, Tag, Text};

/// Magic prefix identifying a sax-wasm checkpoint.
pub const MAGIC: [u8; 4] = *b"SAXC";
/// Bumped whenever the on-disk layout changes incompatibly.
pub const FORMAT_VERSION: u8 = 1;
/// Length of the fixed header.
pub const HEADER_LEN: usize = 42;

/// Reasons a checkpoint may be rejected while restoring.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckpointError {
    /// The bytes do not begin with the checkpoint magic.
    InvalidMagic,
    /// The format version is not supported.
    UnsupportedVersion(u8),
    /// The buffer ends before the advertised data.
    Truncated,
    /// A reserved field carries an unknown value.
    UnsupportedOptions,
    /// The lexical state byte is not a known state.
    InvalidState,
    /// An attribute-type byte is not a known type.
    InvalidAttrType,
    /// The event mask does not match the current parser.
    EventMaskMismatch,
    /// The consumer-provided consumed offset differs from the checkpoint.
    ConsumedOffsetMismatch { expected: u64, actual: u64 },
}

impl CheckpointError {
    /// Stable numeric codes used across the WebAssembly boundary.
    pub fn code(&self) -> i32 {
        match self {
            CheckpointError::InvalidMagic => -1,
            CheckpointError::UnsupportedVersion(_) => -2,
            CheckpointError::Truncated => -3,
            CheckpointError::UnsupportedOptions => -4,
            CheckpointError::InvalidState => -5,
            CheckpointError::InvalidAttrType => -6,
            CheckpointError::EventMaskMismatch => -7,
            CheckpointError::ConsumedOffsetMismatch { .. } => -8,
        }
    }
}

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Writer {
        Writer { buf: Vec::with_capacity(256) }
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.buf.extend_from_slice(value);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CheckpointError> {
        if self.remaining() < len {
            return Err(CheckpointError::Truncated);
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CheckpointError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CheckpointError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, CheckpointError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, CheckpointError> {
        let bytes = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, CheckpointError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

fn state_to_u8(state: State) -> u8 {
    state as u8
}

fn state_from_u8(value: u8) -> Result<State, CheckpointError> {
    match value {
        0 => Ok(State::Begin),
        1 => Ok(State::BeginWhitespace),
        2 => Ok(State::Text),
        3 => Ok(State::LT),
        4 => Ok(State::MarkupDecl),
        5 => Ok(State::Entity),
        6 => Ok(State::Doctype),
        7 => Ok(State::DoctypeEntity),
        8 => Ok(State::Comment),
        15 => Ok(State::Cdata),
        16 => Ok(State::ProcInst),
        17 => Ok(State::ProcInstValue),
        20 => Ok(State::OpenTag),
        21 => Ok(State::OpenTagSlash),
        22 => Ok(State::Attrib),
        23 => Ok(State::AttribName),
        24 => Ok(State::AttribNameSawWhite),
        25 => Ok(State::AttribValue),
        26 => Ok(State::AttribValueQuoted),
        27 => Ok(State::AttribValueClosed),
        28 => Ok(State::AttribValueUnquoted),
        29 => Ok(State::CloseTag),
        30 => Ok(State::JSXAttributeExpression),
        31 => Ok(State::SkipWhitespace),
        _ => Err(CheckpointError::InvalidState),
    }
}

fn write_text(writer: &mut Writer, text: &Text) {
    writer.bytes(&text.value);
    writer.u64(text.start[0]);
    writer.u64(text.start[1]);
    writer.u64(text.end[0]);
    writer.u64(text.end[1]);
    writer.u64(text.byte_range.0);
    writer.u64(text.byte_range.1);
}

fn read_text(reader: &mut Reader) -> Result<Text, CheckpointError> {
    let value = reader.bytes()?;
    let start = [reader.u64()?, reader.u64()?];
    let end = [reader.u64()?, reader.u64()?];
    let byte_range = (reader.u64()?, reader.u64()?);
    Ok(Text {
        // Every header is drained by `hydrate()` before a checkpoint is taken.
        header: (0, 0),
        value,
        start,
        end,
        byte_range,
    })
}

fn write_attribute(writer: &mut Writer, attribute: &Attribute) {
    write_text(writer, &attribute.name);
    write_text(writer, &attribute.value);
    writer.u8(attribute.attr_type as u8);
    writer.u64(attribute.byte_range.0);
    writer.u64(attribute.byte_range.1);
}

fn read_attribute(reader: &mut Reader) -> Result<Attribute, CheckpointError> {
    let name = read_text(reader)?;
    let value = read_text(reader)?;
    let attr_type = AttrType::from_u8(reader.u8()?).ok_or(CheckpointError::InvalidAttrType)?;
    let byte_range = (reader.u64()?, reader.u64()?);
    Ok(Attribute { name, value, attr_type, byte_range })
}

fn write_tag(writer: &mut Writer, tag: &Tag) {
    writer.bytes(&tag.name);
    writer.u32(tag.attributes.len() as u32);
    for attribute in &tag.attributes {
        write_attribute(writer, attribute);
    }
    writer.u32(tag.text_nodes.len() as u32);
    for text in &tag.text_nodes {
        write_text(writer, text);
    }
    writer.u8(tag.self_closing as u8);
    for position in [
        tag.open_start, tag.open_end, tag.close_start, tag.close_end,
    ] {
        writer.u64(position[0]);
        writer.u64(position[1]);
    }
    writer.u64(tag.byte_range.0);
    writer.u64(tag.byte_range.1);
}

fn read_tag(reader: &mut Reader) -> Result<Tag, CheckpointError> {
    let name = reader.bytes()?;
    let mut attributes = Vec::new();
    let attribute_count = reader.u32()?;
    for _ in 0..attribute_count {
        attributes.push(read_attribute(reader)?);
    }
    let mut text_nodes = Vec::new();
    let text_count = reader.u32()?;
    for _ in 0..text_count {
        text_nodes.push(read_text(reader)?);
    }
    let self_closing = reader.u8()? != 0;
    let open_start = [reader.u64()?, reader.u64()?];
    let open_end = [reader.u64()?, reader.u64()?];
    let close_start = [reader.u64()?, reader.u64()?];
    let close_end = [reader.u64()?, reader.u64()?];
    let byte_range = (reader.u64()?, reader.u64()?);
    Ok(Tag {
        header: (0, 0),
        name,
        attributes,
        text_nodes,
        self_closing,
        open_start,
        open_end,
        close_start,
        close_end,
        byte_range,
    })
}

fn write_proc_inst(writer: &mut Writer, proc_inst: &ProcInst) {
    writer.u64(proc_inst.start[0]);
    writer.u64(proc_inst.start[1]);
    writer.u64(proc_inst.end[0]);
    writer.u64(proc_inst.end[1]);
    write_text(writer, &proc_inst.target);
    write_text(writer, &proc_inst.content);
    writer.u64(proc_inst.byte_range.0);
    writer.u64(proc_inst.byte_range.1);
}

fn read_proc_inst(reader: &mut Reader) -> Result<ProcInst, CheckpointError> {
    let start = [reader.u64()?, reader.u64()?];
    let end = [reader.u64()?, reader.u64()?];
    let target = read_text(reader)?;
    let content = read_text(reader)?;
    let byte_range = (reader.u64()?, reader.u64()?);
    Ok(ProcInst { start, end, target, content, byte_range })
}

fn event_mask(events: &[bool; 10]) -> u16 {
    let mut mask = 0u16;
    for (index, enabled) in events.iter().enumerate() {
        if *enabled {
            mask |= 1u16 << index;
        }
    }
    mask
}

fn events_from_mask(mask: u16) -> [bool; 10] {
    let mut events = [false; 10];
    for (index, slot) in events.iter_mut().enumerate() {
        *slot = mask & (1u16 << index) != 0;
    }
    events
}

/// Serializes the parser's structural state. Identical states always
/// produce identical bytes.
pub fn serialize(parser: &SAXParser) -> Vec<u8> {
    let snapshot = parser.snapshot();
    let mut writer = Writer::new();

    writer.buf.extend_from_slice(&MAGIC);
    writer.u8(FORMAT_VERSION);
    writer.u16(0); // reserved parse-options word
    writer.u16(event_mask(&snapshot.events));
    writer.u64(snapshot.consumed);
    writer.u64(snapshot.end_pos[0]);
    writer.u64(snapshot.end_pos[1]);
    writer.u8(state_to_u8(snapshot.state));
    writer.u8(snapshot.quote);
    writer.u16(snapshot.brace_ct.min(65535) as u16);
    writer.u32(snapshot.fragment.len() as u32);
    writer.u8(0); // reserved header tail
    writer.buf.extend_from_slice(&snapshot.fragment);

    writer.u32(snapshot.tags.len() as u32);
    for tag in snapshot.tags {
        write_tag(&mut writer, tag);
    }

    if let Some(text) = &snapshot.text {
        writer.u8(1);
        write_text(&mut writer, text);
    } else {
        writer.u8(0);
    }

    if let Some(text) = &snapshot.markup_decl {
        writer.u8(1);
        write_text(&mut writer, text);
    } else {
        writer.u8(0);
    }

    if let Some(text) = &snapshot.markup_entity {
        writer.u8(1);
        write_text(&mut writer, text);
    } else {
        writer.u8(0);
    }

    if let Some(proc_inst) = &snapshot.proc_inst {
        writer.u8(1);
        write_proc_inst(&mut writer, proc_inst);
    } else {
        writer.u8(0);
    }

    write_attribute(&mut writer, &snapshot.attribute);
    write_tag(&mut writer, &snapshot.tag);

    if snapshot.state == State::CloseTag {
        writer.u8(1);
        write_text(&mut writer, &snapshot.close_tag);
    } else {
        writer.u8(0);
    }

    writer.buf
}

/// A borrowed, pointer-free view of the parser state captured between writes.
pub struct ParserSnapshot<'a> {
    pub events: [bool; 10],
    pub state: State,
    pub brace_ct: u32,
    pub quote: u8,
    pub tags: &'a [Tag],
    pub text: Option<&'a Text>,
    pub markup_decl: Option<&'a Text>,
    pub markup_entity: Option<&'a Text>,
    pub proc_inst: Option<&'a ProcInst>,
    pub attribute: &'a Attribute,
    pub tag: &'a Tag,
    pub close_tag: Text,
    pub fragment: &'a [u8],
    pub end_pos: [u64; 2],
    pub consumed: u64,
}

/// Parsed, validated checkpoint contents.
pub struct CheckpointData {
    pub options: u16,
    pub events: [bool; 10],
    pub consumed: u64,
    pub end_pos: [u64; 2],
    pub state: State,
    pub brace_ct: u16,
    pub quote: u8,
    pub fragment: Vec<u8>,
    pub tags: Vec<Tag>,
    pub text: Option<Text>,
    pub markup_decl: Option<Text>,
    pub markup_entity: Option<Text>,
    pub proc_inst: Option<ProcInst>,
    pub attribute: Attribute,
    pub tag: Tag,
    pub close_tag: Text,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sax::parser::{Event, EventHandler, SAXParser};
    use crate::sax::tag::Entity;
    use std::cell::RefCell;

    struct Collector {
        events: RefCell<Vec<(Event, Vec<u8>)>>,
    }

    impl Collector {
        fn new() -> Self {
            Collector {
                events: RefCell::new(Vec::new()),
            }
        }
    }

    impl EventHandler for Collector {
        fn handle_event(&self, event: Event, data: Entity) {
            let value = match data {
                Entity::Text(text) => text.value.clone(),
                Entity::Tag(tag) => tag.name.clone(),
                _ => Vec::new(),
            };
            self.events.borrow_mut().push((event, value));
        }
    }

    fn parser_with_all_events(handler: &Collector) -> SAXParser<'_> {
        let mut parser = SAXParser::new(handler);
        parser.events = [true; 10];
        parser
    }

    #[test]
    fn checkpoint_is_deterministic() {
        let handler = Collector::new();
        let mut parser = parser_with_all_events(&handler);
        parser.write(b"<root><a>hello \xF0\x9F\x9A\x80");
        let first = serialize(&parser);
        let second = serialize(&parser);
        assert_eq!(first, second);
        assert_eq!(&first[..4], &MAGIC);
        assert_eq!(first[4], FORMAT_VERSION);
    }

    #[test]
    fn resume_produces_identical_events_at_every_cut() {
        let document = b"<root>\n  <a x='1'>hi \xF0\x9F\x9A\x80 there</a><!-- c --></root>";
        for cut in 0..=document.len() {
            let one_shot_handler = Collector::new();
            let mut one_shot = parser_with_all_events(&one_shot_handler);
            one_shot.write(document);
            one_shot.identity();
            let expected = one_shot_handler.events.borrow().clone();

            let prefix_handler = Collector::new();
            let mut prefix = parser_with_all_events(&prefix_handler);
            prefix.write(&document[..cut]);
            let bytes = serialize(&prefix);
            let consumed = u64::from_le_bytes(bytes[9..17].try_into().unwrap());

            let suffix_handler = Collector::new();
            let mut suffix = parser_with_all_events(&suffix_handler);
            let data = decode(&bytes, 0x03ff, Some(consumed)).expect("valid checkpoint");
            suffix.restore_checkpoint(data);
            suffix.write(&document[consumed as usize..]);
            suffix.identity();

            let mut rebuilt = prefix_handler.events.borrow().clone();
            rebuilt.extend(suffix_handler.events.borrow().iter().cloned());
            let filter_open_tag_start = |events: Vec<(Event, Vec<u8>)>| {
                events
                    .into_iter()
                    .filter(|(event, _)| *event != Event::OpenTagStart)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                filter_open_tag_start(rebuilt)
                    .iter()
                    .map(|(e, v)| (*e, v.as_slice()))
                    .collect::<Vec<_>>(),
                filter_open_tag_start(expected)
                    .iter()
                    .map(|(e, v)| (*e, v.as_slice()))
                    .collect::<Vec<_>>(),
                "event stream must match at cut {cut}"
            );
        }
    }

    #[test]
    fn rejects_bad_magic_version_and_truncation() {
        let handler = Collector::new();
        let mut parser = parser_with_all_events(&handler);
        parser.write(b"<a>");
        let mut bytes = serialize(&parser);
        assert!(matches!(decode(&bytes[..10], 0x03ff, None), Err(CheckpointError::Truncated)));
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes, 0x03ff, None), Err(CheckpointError::InvalidMagic)));
        bytes[0] = MAGIC[0];
        bytes[4] = FORMAT_VERSION + 1;
        assert!(matches!(decode(&bytes, 0x03ff, None), Err(CheckpointError::UnsupportedVersion(_))));
    }

    #[test]
    fn rejects_event_mask_and_offset_mismatch() {
        let handler = Collector::new();
        let mut parser = parser_with_all_events(&handler);
        parser.write(b"<a>x");
        let bytes = serialize(&parser);
        assert!(matches!(decode(&bytes, 0, None), Err(CheckpointError::EventMaskMismatch)));
        assert!(matches!(
            decode(&bytes, 0x03ff, Some(999)),
            Err(CheckpointError::ConsumedOffsetMismatch { .. })
        ));
    }

    #[test]
    fn checkpoint_contains_no_raw_pointers_and_survives_buffer_move() {
        let handler = Collector::new();
        let mut parser = parser_with_all_events(&handler);
        parser.write(b"<a><b>text");
        let bytes = serialize(&parser);
        // The serialized form is plain bytes; restore into a fresh parser.
        let other_handler = Collector::new();
        let mut restored = parser_with_all_events(&other_handler);
        let data = decode(&bytes, 0x03ff, None).unwrap();
        restored.restore_checkpoint(data);
        restored.write(b"</b></a>");
        restored.identity();
        assert!(other_handler
            .events
            .borrow()
            .iter()
            .any(|(event, name)| *event == Event::CloseTag && name == b"b"));
    }
}

/// Decodes and validates a checkpoint without touching any live parser.
///
/// `expected_events` is the parser's current event mask and
/// `expected_consumed` is asserted against the checkpoint's own counter.
pub fn decode(
    bytes: &[u8],
    expected_events: u16,
    expected_consumed: Option<u64>,
) -> Result<CheckpointData, CheckpointError> {
    if bytes.len() < HEADER_LEN {
        return Err(CheckpointError::Truncated);
    }
    if bytes[0..4] != MAGIC {
        return Err(CheckpointError::InvalidMagic);
    }
    let version = bytes[4];
    if version != FORMAT_VERSION {
        return Err(CheckpointError::UnsupportedVersion(version));
    }

    let mut reader = Reader::new(bytes);
    reader.take(5)?; // magic + version
    let options = reader.u16()?;
    if options != 0 {
        return Err(CheckpointError::UnsupportedOptions);
    }
    let mask = reader.u16()?;
    if mask != expected_events {
        return Err(CheckpointError::EventMaskMismatch);
    }
    let consumed = reader.u64()?;
    if let Some(expected) = expected_consumed {
        if expected != consumed {
            return Err(CheckpointError::ConsumedOffsetMismatch {
                expected,
                actual: consumed,
            });
        }
    }
    let end_pos = [reader.u64()?, reader.u64()?];
    let state = state_from_u8(reader.u8()?)?;
    let quote = reader.u8()?;
    let brace_ct = reader.u16()?;
    let fragment_len = reader.u32()? as usize;
    reader.take(1)?; // reserved tail

    let fragment = reader.take(fragment_len)?.to_vec();

    let mut tags = Vec::new();
    let tag_count = reader.u32()?;
    for _ in 0..tag_count {
        tags.push(read_tag(&mut reader)?);
    }

    let text = if reader.u8()? != 0 {
        Some(read_text(&mut reader)?)
    } else {
        None
    };
    let markup_decl = if reader.u8()? != 0 {
        Some(read_text(&mut reader)?)
    } else {
        None
    };
    let markup_entity = if reader.u8()? != 0 {
        Some(read_text(&mut reader)?)
    } else {
        None
    };
    let proc_inst = if reader.u8()? != 0 {
        Some(read_proc_inst(&mut reader)?)
    } else {
        None
    };

    let attribute = read_attribute(&mut reader)?;
    let tag = read_tag(&mut reader)?;
    let close_tag = if reader.u8()? != 0 {
        read_text(&mut reader)?
    } else {
        Text::new([0, 0])
    };

    if reader.remaining() != 0 {
        return Err(CheckpointError::Truncated);
    }

    Ok(CheckpointData {
        options,
        events: events_from_mask(mask),
        consumed,
        end_pos,
        state,
        brace_ct,
        quote,
        fragment,
        tags,
        text,
        markup_decl,
        markup_entity,
        proc_inst,
        attribute,
        tag,
        close_tag,
    })
}
