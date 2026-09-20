use std::mem;
use std::ptr;
use std::slice;

use crate::sax::checkpoint;
use crate::sax::parser::*;
use crate::sax::tag::*;

static mut SAX: *mut SAXParser = 0 as *mut SAXParser;
static mut CHECKPOINT: Vec<u8> = Vec::new();
pub struct SaxEventHandler;

impl SaxEventHandler {
    pub fn new() -> Self {
        SaxEventHandler
    }
}

impl EventHandler for SaxEventHandler {
    fn handle_event(&self, event: Event, data: Entity) {
        let ptr = match data {
            Entity::Attribute(attribute) => ptr::from_ref(attribute) as *const u8,
            Entity::ProcInst(proc_inst) => ptr::from_ref(proc_inst) as *const u8,
            Entity::Tag(tag) => ptr::from_ref(tag) as *const u8,
            Entity::Text(text) => ptr::from_ref(text) as *const u8,
        };
        unsafe { event_listener(1 << event as u32, ptr) };
    }
}

fn generate_event_lookup(events: u32) -> [bool; 10] {
    let mut event_lookup = [false; 10];
    for i in 0..10 {
        event_lookup[i] = events & (1 << i) != 0;
    }
    event_lookup
}

#[no_mangle]
pub unsafe extern "C" fn parser(events: u32) {
    if SAX == 0 as *mut SAXParser {
        let event_handler = Box::leak(Box::new(SaxEventHandler::new()));
        let sax_parse = SAXParser::new(event_handler);
        SAX = mem::transmute(Box::new(sax_parse));
    }
    (*SAX).events = generate_event_lookup(events);
}

#[no_mangle]
pub unsafe extern "C" fn write(ptr: *const u8, length: usize) {
    let document = slice::from_raw_parts(ptr, length);
    (*SAX).write(document);
}

#[no_mangle]
pub unsafe extern "C" fn end() {
    (*SAX).identity();
}

/// Serializes the current parser state into the module-owned checkpoint
/// buffer. Returns the byte length of the checkpoint.
#[no_mangle]
pub unsafe extern "C" fn save_checkpoint() -> usize {
    CHECKPOINT = checkpoint::serialize(&*SAX);
    CHECKPOINT.len()
}

/// Pointer to the most recently produced checkpoint bytes.
#[no_mangle]
pub unsafe extern "C" fn checkpoint_ptr() -> *const u8 {
    CHECKPOINT.as_ptr()
}

/// Restores parser state from a checkpoint. `events` is the active event
/// mask and `expected_consumed`, when non-null, is the caller's view of how
/// many stream bytes were consumed. Returns 0 on success or a negative
/// `CheckpointError` code. On failure the current parser is untouched.
#[no_mangle]
pub unsafe extern "C" fn restore_checkpoint(
    pointer: *const u8,
    length: usize,
    events: u32,
    expected_consumed: *const u64,
) -> i32 {
    let bytes = slice::from_raw_parts(pointer, length);
    let expected = if expected_consumed.is_null() {
        None
    } else {
        Some(*expected_consumed)
    };
    let mask = (events & 0xFFFF) as u16;
    match checkpoint::decode(bytes, mask, expected) {
        Ok(data) => {
            (*SAX).restore_checkpoint(data);
            0
        }
        Err(error) => error.code(),
    }
}

#[link(wasm_import_module = "env")]
extern "C" {
    fn event_listener(event: u32, ptr: *const u8);
}
