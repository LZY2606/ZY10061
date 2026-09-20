use super::parser::{SAXParser, State};
use super::tag::{Attribute, ProcInst, Tag, Text};

pub const CHECKPOINT_MAGIC: [u8; 8] = *b"SAXWCKPT";
pub const CHECKPOINT_VERSION: u8 = 1;
const EVENT_MASK: u32 = 0b11_1111_1111;

pub type CheckpointResult<T> = Result<T, CheckpointError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    InvalidMagic,
    UnsupportedVersion,
    Truncated,
    InvalidLength,
    InvalidEvents,
    InvalidState,
    InvalidQuote,
    InvalidOffset,
    InvalidUtf8,
}

impl CheckpointError {
    pub fn code(self) -> u32 {
        match self {
            CheckpointError::InvalidMagic => 1,
            CheckpointError::UnsupportedVersion => 2,
            CheckpointError::Truncated => 3,
            CheckpointError::InvalidLength => 4,
            CheckpointError::InvalidEvents => 5,
            CheckpointError::InvalidState => 6,
            CheckpointError::InvalidQuote => 7,
            CheckpointError::InvalidOffset => 8,
            CheckpointError::InvalidUtf8 => 9,
        }
    }
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Writer { bytes: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn bool(&mut self, value: bool) {
        self.u8(value as u8);
    }

    fn bytes(&mut self, value: &[u8]) -> CheckpointResult<()> {
        let length = u32::try_from(value.len()).map_err(|_| CheckpointError::InvalidLength)?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> CheckpointResult<&'a [u8]> {
        let end = self.position.checked_add(length).ok_or(CheckpointError::Truncated)?;
        let value = self.bytes.get(self.position..end).ok_or(CheckpointError::Truncated)?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> CheckpointResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> CheckpointResult<u32> {
        let value = u32::from_le_bytes(self.take(4)?.try_into().unwrap());
        Ok(value)
    }

    fn u64(&mut self) -> CheckpointResult<u64> {
        let value = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        Ok(value)
    }

    fn bool(&mut self) -> CheckpointResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CheckpointError::Truncated),
        }
    }

    fn bytes(&mut self) -> CheckpointResult<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }
}

pub fn serialize(parser: &SAXParser) -> CheckpointResult<Vec<u8>> {
    let mut snapshot = parser.clone_without_handler();
    snapshot.freeze_for_checkpoint();
    let parser = &snapshot;
    let mut writer = Writer::new();
    writer.bytes.extend_from_slice(&CHECKPOINT_MAGIC);
    writer.u8(CHECKPOINT_VERSION);
    writer.u8(0);
    writer.u8(0);
    writer.u8(0);
    writer.u32(events_mask(&parser.events));
    writer.u64(parser.consumed_offset);
    writer.u64(parser.end_pos[0]);
    writer.u64(parser.end_pos[1]);
    writer.u8(state_code(parser.state));
    writer.u32(parser.brace_ct);
    writer.u8(parser.quote);
    writer.bool(parser.open_tag_start_dispatched);
    writer.bytes(&parser.fragment)?;
    write_text(&mut writer, parser.text.as_ref())?;
    write_text(&mut writer, parser.markup_decl.as_ref())?;
    write_text(&mut writer, parser.markup_entity.as_ref())?;
    write_text(&mut writer, Some(&parser.close_tag))?;
    write_attribute(&mut writer, &parser.attribute)?;
    write_proc_inst(&mut writer, parser.proc_inst.as_ref())?;
    write_tag(&mut writer, &parser.tag)?;

    let tags = &parser.tags;
    writer.u32(tags.len() as u32);
    for tag in tags {
        write_tag(&mut writer, tag)?;
    }
    Ok(writer.bytes)
}

pub fn deserialize(data: &[u8], events: u32, consumed_offset: u64) -> CheckpointResult<SAXParser<'static>> {
    let mut reader = Reader::new(data);
    let magic = reader.take(CHECKPOINT_MAGIC.len())?;
    if magic != CHECKPOINT_MAGIC {
        return Err(CheckpointError::InvalidMagic);
    }
    if reader.u8()? != CHECKPOINT_VERSION {
        return Err(CheckpointError::UnsupportedVersion);
    }
    if reader.take(3)? != [0, 0, 0] {
        return Err(CheckpointError::Truncated);
    }

    let checkpoint_events = reader.u32()?;
    if checkpoint_events & !EVENT_MASK != 0 {
        return Err(CheckpointError::InvalidEvents);
    }
    if checkpoint_events != events {
        return Err(CheckpointError::InvalidEvents);
    }
    let checkpoint_offset = reader.u64()?;
    if checkpoint_offset != consumed_offset {
        return Err(CheckpointError::InvalidOffset);
    }
    let line = reader.u64()?;
    let character = reader.u64()?;
    let state = state_from_code(reader.u8()?)?;
    let brace_ct = reader.u32()?;
    let quote = reader.u8()?;
    if !matches!(quote, 0 | b'"' | b'\'') {
        return Err(CheckpointError::InvalidQuote);
    }
    let open_tag_start_dispatched = reader.bool()?;

    let fragment = reader.bytes()?.to_vec();
    validate_utf8_fragment(&fragment)?;
    let text = optional_text(&mut reader)?;
    let markup_decl = optional_text(&mut reader)?;
    let markup_entity = optional_text(&mut reader)?;
    let close_tag = read_text(&mut reader)?;
    let attribute = read_attribute(&mut reader)?;
    let proc_inst = read_proc_inst(&mut reader)?;
    let tag = read_tag(&mut reader)?;
    let tag_count = reader.u32()? as usize;
    let mut tags = Vec::with_capacity(tag_count);
    for _ in 0..tag_count {
        tags.push(read_tag(&mut reader)?);
    }
    if reader.position != data.len() {
        return Err(CheckpointError::Truncated);
    }

    Ok(SAXParser::from_checkpoint(
        checkpoint_events,
        checkpoint_offset,
        [line, character],
        state,
        brace_ct,
        quote,
        open_tag_start_dispatched,
        fragment,
        text,
        markup_decl,
        markup_entity,
        close_tag,
        attribute,
        proc_inst,
        tag,
        tags,
    ))
}

fn write_text(writer: &mut Writer, text: Option<&Text>) -> CheckpointResult<()> {
    match text {
        Some(text) => {
            writer.bool(true);
            writer.u64(text.start[0]);
            writer.u64(text.start[1]);
            writer.u64(text.end[0]);
            writer.u64(text.end[1]);
            writer.u64(text.byte_range.0);
            writer.u64(text.byte_range.1);
            writer.bytes(&text.value)?;
        }
        None => writer.bool(false),
    }
    Ok(())
}

fn read_text(reader: &mut Reader) -> CheckpointResult<Text> {
    if !reader.bool()? {
        return Err(CheckpointError::Truncated);
    }
    let start = [reader.u64()?, reader.u64()?];
    let end = [reader.u64()?, reader.u64()?];
    let byte_range = (reader.u64()?, reader.u64()?);
    let value = reader.bytes()?.to_vec();
    let mut text = Text::new(start);
    text.end = end;
    text.byte_range = byte_range;
    text.value = value;
    text.header = (usize::MAX, usize::MAX);
    Ok(text)
}

fn optional_text(reader: &mut Reader) -> CheckpointResult<Option<Text>> {
    Ok(if reader.bytes.get(reader.position).copied() == Some(1) {
        Some(read_text(reader)?)
    } else {
        reader.u8()?;
        None
    })
}

fn write_attribute(writer: &mut Writer, attribute: &Attribute) -> CheckpointResult<()> {
    write_text(writer, Some(&attribute.name))?;
    write_text(writer, Some(&attribute.value))?;
    writer.u8(attr_type_code(attribute.attr_type));
    writer.u64(attribute.byte_range.0);
    writer.u64(attribute.byte_range.1);
    Ok(())
}

fn read_attribute(reader: &mut Reader) -> CheckpointResult<Attribute> {
    let name = read_text(reader)?;
    let value = read_text(reader)?;
    let attr_type = attr_type_from_code(reader.u8()?)?;
    let byte_range = (reader.u64()?, reader.u64()?);
    Ok(Attribute { name, value, attr_type, byte_range })
}

fn write_proc_inst(writer: &mut Writer, proc_inst: Option<&ProcInst>) -> CheckpointResult<()> {
    match proc_inst {
        Some(proc_inst) => {
            writer.bool(true);
            writer.u64(proc_inst.start[0]);
            writer.u64(proc_inst.start[1]);
            writer.u64(proc_inst.end[0]);
            writer.u64(proc_inst.end[1]);
            writer.u64(proc_inst.byte_range.0);
            writer.u64(proc_inst.byte_range.1);
            write_text(writer, Some(&proc_inst.target))?;
            write_text(writer, Some(&proc_inst.content))?;
        }
        None => writer.bool(false),
    }
    Ok(())
}

fn read_proc_inst(reader: &mut Reader) -> CheckpointResult<Option<ProcInst>> {
    if !reader.bool()? {
        return Ok(None);
    }
    let start = [reader.u64()?, reader.u64()?];
    let end = [reader.u64()?, reader.u64()?];
    let byte_range = (reader.u64()?, reader.u64()?);
    let target = read_text(reader)?;
    let content = read_text(reader)?;
    Ok(Some(ProcInst { start, end, target, content, byte_range }))
}

fn write_tag(writer: &mut Writer, tag: &Tag) -> CheckpointResult<()> {
    writer.u64(tag.open_start[0]);
    writer.u64(tag.open_start[1]);
    writer.u64(tag.open_end[0]);
    writer.u64(tag.open_end[1]);
    writer.u64(tag.close_start[0]);
    writer.u64(tag.close_start[1]);
    writer.u64(tag.close_end[0]);
    writer.u64(tag.close_end[1]);
    writer.bool(tag.self_closing);
    writer.u64(tag.byte_range.0);
    writer.u64(tag.byte_range.1);
    writer.bool(tag.pending_name);
    writer.bytes(&tag.name)?;

    writer.u32(tag.attributes.len() as u32);
    for attribute in &tag.attributes {
        write_attribute(writer, attribute)?;
    }
    writer.u32(tag.text_nodes.len() as u32);
    for text in &tag.text_nodes {
        write_text(writer, Some(text))?;
    }
    Ok(())
}

fn read_tag(reader: &mut Reader) -> CheckpointResult<Tag> {
    let open_start = [reader.u64()?, reader.u64()?];
    let open_end = [reader.u64()?, reader.u64()?];
    let close_start = [reader.u64()?, reader.u64()?];
    let close_end = [reader.u64()?, reader.u64()?];
    let self_closing = reader.bool()?;
    let byte_range = (reader.u64()?, reader.u64()?);
    let pending_name = reader.bool()?;
    let name = reader.bytes()?.to_vec();

    let attribute_count = reader.u32()? as usize;
    let mut attributes = Vec::with_capacity(attribute_count);
    for _ in 0..attribute_count {
        attributes.push(read_attribute(reader)?);
    }
    let text_count = reader.u32()? as usize;
    let mut text_nodes = Vec::with_capacity(text_count);
    for _ in 0..text_count {
        text_nodes.push(read_text(reader)?);
    }

    Ok(Tag {
        name,
        attributes,
        text_nodes,
        self_closing,
        open_start,
        open_end,
        close_start,
        close_end,
        header: if pending_name { (usize::MAX, 0) } else { (usize::MAX, usize::MAX) },
        byte_range,
        pending_name,
    })
}

fn validate_utf8_fragment(bytes: &[u8]) -> CheckpointResult<()> {
    match bytes.first() {
        None => Ok(()),
        Some(first @ 0xC0..=0xDF) if bytes.len() < 2 && continuation_bytes(bytes) => {
            let _ = first;
            Ok(())
        }
        Some(first @ 0xE0..=0xEF) if bytes.len() < 3 && continuation_bytes(bytes) => {
            let _ = first;
            Ok(())
        }
        Some(first @ 0xF0..=0xF7) if bytes.len() < 4 && continuation_bytes(bytes) => {
            let _ = first;
            Ok(())
        }
        _ => Err(CheckpointError::InvalidUtf8),
    }
}

fn continuation_bytes(bytes: &[u8]) -> bool {
    bytes[1..].iter().all(|byte| (0x80..=0xBF).contains(byte))
}

fn state_code(state: State) -> u8 {
    state as u8
}

fn events_mask(events: &[bool; 10]) -> u32 {
    events
        .iter()
        .enumerate()
        .fold(0, |mask, (index, enabled)| mask | ((*enabled as u32) << index))
}

fn state_from_code(code: u8) -> CheckpointResult<State> {
    State::from_checkpoint_code(code).ok_or(CheckpointError::InvalidState)
}

fn attr_type_code(attr_type: super::tag::AttrType) -> u8 {
    match attr_type {
        super::tag::AttrType::NoValue => 0,
        super::tag::AttrType::JSX => 1,
        super::tag::AttrType::NoQuotes => 2,
        super::tag::AttrType::SingleQuoted => 4,
        super::tag::AttrType::DoubleQuoted => 8,
    }
}

fn attr_type_from_code(code: u8) -> CheckpointResult<super::tag::AttrType> {
    match code {
        0 => Ok(super::tag::AttrType::NoValue),
        1 => Ok(super::tag::AttrType::JSX),
        2 => Ok(super::tag::AttrType::NoQuotes),
        4 => Ok(super::tag::AttrType::SingleQuoted),
        8 => Ok(super::tag::AttrType::DoubleQuoted),
        _ => Err(CheckpointError::InvalidState),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sax::parser::{Event, EventHandler, SAXParser};
    use crate::sax::tag::Entity;
    use std::cell::RefCell;

    struct Handler {
        events: RefCell<Vec<u8>>,
    }

    impl EventHandler for Handler {
        fn handle_event(&self, _event: Event, _data: Entity) {
            self.events.borrow_mut().push(1);
        }
    }

    #[test]
    fn deterministic_checkpoint_round_trip_validates_options_and_offset() {
        let handler = Handler { events: RefCell::new(Vec::new()) };
        let mut parser = SAXParser::new(&handler);
        parser.events = [true; 10];
        parser.write(b"<r>");
        let first = serialize(&parser).unwrap();
        let second = serialize(&parser).unwrap();
        assert_eq!(first, second);
        assert_eq!(&first[..8], CHECKPOINT_MAGIC);
        assert_eq!(first[8], CHECKPOINT_VERSION);

        let restored = deserialize(&first, 1023, parser.consumed_offset).unwrap();
        assert_eq!(state_code(restored.state), state_code(parser.state));
        assert_eq!(restored.consumed_offset, parser.consumed_offset);

        assert!(matches!(
            deserialize(&first, 1023, parser.consumed_offset + 1),
            Err(CheckpointError::InvalidOffset)
        ));
        assert!(matches!(
            deserialize(&first, 1022, parser.consumed_offset),
            Err(CheckpointError::InvalidEvents)
        ));
        let mut bad = first.clone();
        bad[8] = 99;
        assert!(matches!(deserialize(&bad, 1023, parser.consumed_offset), Err(CheckpointError::UnsupportedVersion)));
    }
}
