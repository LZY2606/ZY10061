use std::alloc::{alloc, dealloc, Layout};
use std::ptr;
use std::slice;

use crate::sax::checkpoint;
use crate::sax::parser::*;
use crate::sax::tag::*;

static mut SAX: *mut SAXParser = 0 as *mut SAXParser;
static mut CHECKPOINT: Option<Vec<u8>> = None;
static mut CHECKPOINT_LENGTH: usize = 0;

#[link(wasm_import_module = "env")]
extern "C" {
    fn event_listener(event: u32, ptr: *const u8);
}

pub struct SaxEventHandler;

impl SaxEventHandler {
    pub const fn new() -> Self {
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

static EVENT_HANDLER: SaxEventHandler = SaxEventHandler;
#[no_mangle]
pub unsafe extern "C" fn parser(events: u32) {
    if SAX == 0 as *mut SAXParser {
        let sax_parse = SAXParser::new(&EVENT_HANDLER);
        SAX = Box::into_raw(Box::new(sax_parse));
    }
    (*SAX).events = generate_event_lookup(events);
}

#[no_mangle]
pub unsafe extern "C" fn write(ptr: *const u8, length: usize) -> u64 {
    let document = slice::from_raw_parts(ptr, length);
    (*SAX).write(document);
    (*SAX).consumed_offset
}

#[no_mangle]
pub unsafe extern "C" fn end() {
    (*SAX).identity();
}

#[no_mangle]
pub unsafe extern "C" fn allocate(length: usize) -> *mut u8 {
    let layout = match Layout::from_size_align(length, 1) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };
    alloc(layout)
}

#[no_mangle]
pub unsafe extern "C" fn deallocate(ptr: *mut u8, length: usize) {
    if !ptr.is_null() {
        dealloc(ptr, Layout::from_size_align_unchecked(length, 1));
    }
}

#[no_mangle]
pub unsafe extern "C" fn checkpoint() -> u32 {
    match checkpoint::serialize(&*SAX) {
        Ok(bytes) => {
            let pointer = bytes.as_ptr() as u32;
            CHECKPOINT_LENGTH = bytes.len();
            CHECKPOINT = Some(bytes);
            pointer
        }
        Err(error) => 0x8000_0000 | error.code(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn checkpoint_length() -> u32 {
    CHECKPOINT_LENGTH as u32
}

#[no_mangle]
pub unsafe extern "C" fn resume(
    ptr: *const u8,
    length: usize,
    events: u32,
    consumed_offset_lo: u32,
    consumed_offset_hi: u32,
) -> u32 {
    let data = slice::from_raw_parts(ptr, length);
    let consumed_offset = u64::from(consumed_offset_lo) | (u64::from(consumed_offset_hi) << 32);

    let mut restored = match checkpoint::deserialize(data, events, consumed_offset) {
        Ok(parser) => parser,
        Err(error) => return error.code(),
    };
    restored.set_event_handler(&EVENT_HANDLER);

    if SAX != 0 as *mut SAXParser {
        drop(Box::from_raw(SAX));
    }
    SAX = Box::into_raw(Box::new(restored));
    0
}

fn generate_event_lookup(events: u32) -> [bool; 10] {
    let mut event_lookup = [false; 10];
    for i in 0..10 {
        event_lookup[i] = events & (1 << i) != 0;
    }
    event_lookup
}
