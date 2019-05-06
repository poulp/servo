/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::BlobBinding::BlobMethods;
use crate::dom::bindings::codegen::Bindings::FileReaderBinding::{
    self, FileReaderConstants, FileReaderMethods,
};
use crate::dom::bindings::codegen::UnionTypes::StringOrObject;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::{reflect_dom_object, DomObject};
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::blob::Blob;
use crate::dom::domexception::{DOMErrorName, DOMException};
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::progressevent::ProgressEvent;
use crate::task::TaskCanceller;
use crate::task_source::file_reading::{FileReadingTask, FileReadingTaskSource};
use crate::task_source::{TaskSource, TaskSourceName};
use base64;
use dom_struct::dom_struct;
use encoding_rs::{Encoding, UTF_8};
use js::jsapi::Heap;
use js::jsapi::JSAutoRealm;
use js::jsapi::JSContext;
use js::jsapi::JSObject;
use js::jsval::{self, JSVal};
use js::typedarray::{ArrayBuffer, CreateWith};
use mime::{self, Mime};
use servo_atoms::Atom;
use std::cell::Cell;
use std::ptr;
use std::sync::Arc;
use std::thread;

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub enum FileReaderFunction {
    ReadAsText,
    ReadAsDataUrl,
    ReadAsArrayBuffer,
}

pub type TrustedFileReader = Trusted<FileReader>;

#[derive(Clone, MallocSizeOf)]
pub struct ReadMetaData {
    pub blobtype: String,
    pub label: Option<String>,
    pub function: FileReaderFunction,
}

impl ReadMetaData {
    pub fn new(
        blobtype: String,
        label: Option<String>,
        function: FileReaderFunction,
    ) -> ReadMetaData {
        ReadMetaData {
            blobtype: blobtype,
            label: label,
            function: function,
        }
    }
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub struct GenerationId(u32);

#[repr(u16)]
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub enum FileReaderReadyState {
    Empty = FileReaderConstants::EMPTY,
    Loading = FileReaderConstants::LOADING,
    Done = FileReaderConstants::DONE,
}

#[derive(JSTraceable, MallocSizeOf)]
pub enum FileReaderResult {
    ArrayBuffer(#[ignore_malloc_size_of = "mozjs"] Heap<JSVal>),
    String(DOMString),
}

pub struct FileReaderSharedFunctionality;

impl FileReaderSharedFunctionality {
    pub fn dataurl_format(blob_contents: &[u8], blob_type: String) -> DOMString {
        let base64 = base64::encode(&blob_contents);

        let dataurl = if blob_type.is_empty() {
            format!("data:base64,{}", base64)
        } else {
            format!("data:{};base64,{}", blob_type, base64)
        };

        DOMString::from(dataurl)
    }

    pub fn text_decode(
        blob_contents: &[u8],
        blob_type: &str,
        blob_label: &Option<String>,
    ) -> DOMString {
        //https://w3c.github.io/FileAPI/#encoding-determination
        // Steps 1 & 2 & 3
        let mut encoding = blob_label
            .as_ref()
            .map(|string| string.as_bytes())
            .and_then(Encoding::for_label);

        // Step 4 & 5
        encoding = encoding.or_else(|| {
            let resultmime = blob_type.parse::<Mime>().ok();
            resultmime.and_then(|mime| {
                mime.params()
                    .find(|(ref k, _)| &mime::CHARSET == k)
                    .and_then(|(_, ref v)| Encoding::for_label(v.as_ref().as_bytes()))
            })
        });

        // Step 6
        let enc = encoding.unwrap_or(UTF_8);

        let convert = blob_contents;
        // Step 7
        let (output, _, _) = enc.decode(convert);
        DOMString::from(output)
    }
}

#[dom_struct]
pub struct FileReader {
    eventtarget: EventTarget,
    ready_state: Cell<FileReaderReadyState>,
    error: MutNullableDom<DOMException>,
    result: DomRefCell<Option<FileReaderResult>>,
    generation_id: Cell<GenerationId>,
}

impl FileReader {
    pub fn new_inherited() -> FileReader {
        FileReader {
            eventtarget: EventTarget::new_inherited(),
            ready_state: Cell::new(FileReaderReadyState::Empty),
            error: MutNullableDom::new(None),
            result: DomRefCell::new(None),
            generation_id: Cell::new(GenerationId(0)),
        }
    }

    pub fn new(global: &GlobalScope) -> DomRoot<FileReader> {
        reflect_dom_object(
            Box::new(FileReader::new_inherited()),
            global,
            FileReaderBinding::Wrap,
        )
    }

    pub fn Constructor(global: &GlobalScope) -> Fallible<DomRoot<FileReader>> {
        Ok(FileReader::new(global))
    }

    //https://w3c.github.io/FileAPI/#dfn-error-steps
    pub fn process_read_error(
        filereader: TrustedFileReader,
        gen_id: GenerationId,
        error: DOMErrorName,
    ) {
        let fr = filereader.root();

        macro_rules! return_on_abort(
            () => (
                if gen_id != fr.generation_id.get() {
                    return
                }
            );
        );

        return_on_abort!();
        // Step 1
        fr.change_ready_state(FileReaderReadyState::Done);
        *fr.result.borrow_mut() = None;

        let exception = DOMException::new(&fr.global(), error);
        fr.error.set(Some(&exception));

        fr.dispatch_progress_event(atom!("error"), 0, None);
        return_on_abort!();
        // Step 3
        fr.dispatch_progress_event(atom!("loadend"), 0, None);
        return_on_abort!();
        // Step 4
        fr.terminate_ongoing_reading();
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsText
    pub fn process_read_data(filereader: TrustedFileReader, gen_id: GenerationId) {
        let fr = filereader.root();

        macro_rules! return_on_abort(
            () => (
                if gen_id != fr.generation_id.get() {
                    return
                }
            );
        );
        return_on_abort!();
        //FIXME Step 7 send current progress
        fr.dispatch_progress_event(atom!("progress"), 0, None);
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsText
    pub fn process_read(filereader: TrustedFileReader, gen_id: GenerationId) {
        let fr = filereader.root();

        macro_rules! return_on_abort(
            () => (
                if gen_id != fr.generation_id.get() {
                    return
                }
            );
        );
        return_on_abort!();
        // Step 6
        fr.dispatch_progress_event(atom!("loadstart"), 0, None);
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsText
    #[allow(unsafe_code)]
    pub fn process_read_eof(
        filereader: TrustedFileReader,
        gen_id: GenerationId,
        data: ReadMetaData,
        blob_contents: Arc<Vec<u8>>,
    ) {
        let fr = filereader.root();

        macro_rules! return_on_abort(
            () => (
                if gen_id != fr.generation_id.get() {
                    return
                }
            );
        );

        return_on_abort!();
        // Step 8.1
        fr.change_ready_state(FileReaderReadyState::Done);
        // Step 8.2

        match data.function {
            FileReaderFunction::ReadAsDataUrl => {
                FileReader::perform_readasdataurl(&fr.result, data, &blob_contents)
            },
            FileReaderFunction::ReadAsText => {
                FileReader::perform_readastext(&fr.result, data, &blob_contents)
            },
            FileReaderFunction::ReadAsArrayBuffer => {
                let _ac = JSAutoRealm::new(fr.global().get_cx(), *fr.reflector().get_jsobject());
                FileReader::perform_readasarraybuffer(
                    &fr.result,
                    fr.global().get_cx(),
                    data,
                    &blob_contents,
                )
            },
        };

        // Step 8.3
        fr.dispatch_progress_event(atom!("load"), 0, None);
        return_on_abort!();
        // Step 8.4
        if fr.ready_state.get() != FileReaderReadyState::Loading {
            fr.dispatch_progress_event(atom!("loadend"), 0, None);
        }
        return_on_abort!();
        // Step 9
        fr.terminate_ongoing_reading();
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsText
    fn perform_readastext(
        result: &DomRefCell<Option<FileReaderResult>>,
        data: ReadMetaData,
        blob_bytes: &[u8],
    ) {
        let blob_label = &data.label;
        let blob_type = &data.blobtype;

        let output = FileReaderSharedFunctionality::text_decode(blob_bytes, blob_type, blob_label);
        *result.borrow_mut() = Some(FileReaderResult::String(output));
    }

    //https://w3c.github.io/FileAPI/#dfn-readAsDataURL
    fn perform_readasdataurl(
        result: &DomRefCell<Option<FileReaderResult>>,
        data: ReadMetaData,
        bytes: &[u8],
    ) {
        let output = FileReaderSharedFunctionality::dataurl_format(bytes, data.blobtype);

        *result.borrow_mut() = Some(FileReaderResult::String(output));
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsArrayBuffer
    #[allow(unsafe_code)]
    fn perform_readasarraybuffer(
        result: &DomRefCell<Option<FileReaderResult>>,
        cx: *mut JSContext,
        _: ReadMetaData,
        bytes: &[u8],
    ) {
        unsafe {
            rooted!(in(cx) let mut array_buffer = ptr::null_mut::<JSObject>());
            assert!(
                ArrayBuffer::create(cx, CreateWith::Slice(bytes), array_buffer.handle_mut())
                    .is_ok()
            );

            *result.borrow_mut() = Some(FileReaderResult::ArrayBuffer(Heap::default()));

            if let Some(FileReaderResult::ArrayBuffer(ref mut heap)) = *result.borrow_mut() {
                heap.set(jsval::ObjectValue(array_buffer.get()));
            };
        }
    }
}

impl FileReaderMethods for FileReader {
    // https://w3c.github.io/FileAPI/#dfn-onloadstart
    event_handler!(loadstart, GetOnloadstart, SetOnloadstart);

    // https://w3c.github.io/FileAPI/#dfn-onprogress
    event_handler!(progress, GetOnprogress, SetOnprogress);

    // https://w3c.github.io/FileAPI/#dfn-onload
    event_handler!(load, GetOnload, SetOnload);

    // https://w3c.github.io/FileAPI/#dfn-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    // https://w3c.github.io/FileAPI/#dfn-onerror
    event_handler!(error, GetOnerror, SetOnerror);

    // https://w3c.github.io/FileAPI/#dfn-onloadend
    event_handler!(loadend, GetOnloadend, SetOnloadend);

    // https://w3c.github.io/FileAPI/#dfn-readAsArrayBuffer
    fn ReadAsArrayBuffer(&self, blob: &Blob) -> ErrorResult {
        self.read(FileReaderFunction::ReadAsArrayBuffer, blob, None)
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsDataURL
    fn ReadAsDataURL(&self, blob: &Blob) -> ErrorResult {
        self.read(FileReaderFunction::ReadAsDataUrl, blob, None)
    }

    // https://w3c.github.io/FileAPI/#dfn-readAsText
    fn ReadAsText(&self, blob: &Blob, label: Option<DOMString>) -> ErrorResult {
        self.read(FileReaderFunction::ReadAsText, blob, label)
    }

    // https://w3c.github.io/FileAPI/#dfn-abort
    fn Abort(&self) {
        // Step 2
        if self.ready_state.get() == FileReaderReadyState::Loading {
            self.change_ready_state(FileReaderReadyState::Done);
        }
        // Steps 1 & 3
        *self.result.borrow_mut() = None;

        let exception = DOMException::new(&self.global(), DOMErrorName::AbortError);
        self.error.set(Some(&exception));

        self.terminate_ongoing_reading();
        // Steps 5 & 6
        self.dispatch_progress_event(atom!("abort"), 0, None);
        self.dispatch_progress_event(atom!("loadend"), 0, None);
    }

    // https://w3c.github.io/FileAPI/#dfn-error
    fn GetError(&self) -> Option<DomRoot<DOMException>> {
        self.error.get()
    }

    #[allow(unsafe_code)]
    // https://w3c.github.io/FileAPI/#dfn-result
    unsafe fn GetResult(&self, _: *mut JSContext) -> Option<StringOrObject> {
        self.result.borrow().as_ref().map(|r| match *r {
            FileReaderResult::String(ref string) => StringOrObject::String(string.clone()),
            FileReaderResult::ArrayBuffer(ref arr_buffer) => {
                let result = RootedTraceableBox::new(Heap::default());
                result.set((*arr_buffer.ptr.get()).to_object());
                StringOrObject::Object(result)
            },
        })
    }

    // https://w3c.github.io/FileAPI/#dfn-readyState
    fn ReadyState(&self) -> u16 {
        self.ready_state.get() as u16
    }
}

impl FileReader {
    fn dispatch_progress_event(&self, type_: Atom, loaded: u64, total: Option<u64>) {
        let progressevent = ProgressEvent::new(
            &self.global(),
            type_,
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
            total.is_some(),
            loaded,
            total.unwrap_or(0),
        );
        progressevent.upcast::<Event>().fire(self.upcast());
    }

    fn terminate_ongoing_reading(&self) {
        let GenerationId(prev_id) = self.generation_id.get();
        self.generation_id.set(GenerationId(prev_id + 1));
    }

    fn read(
        &self,
        function: FileReaderFunction,
        blob: &Blob,
        label: Option<DOMString>,
    ) -> ErrorResult {
        // Step 1
        if self.ready_state.get() == FileReaderReadyState::Loading {
            return Err(Error::InvalidState);
        }

        // Step 2
        self.change_ready_state(FileReaderReadyState::Loading);

        // Step 3
        let blob_contents = Arc::new(blob.get_bytes().unwrap_or(vec![]));

        let type_ = blob.Type();

        let load_data = ReadMetaData::new(String::from(type_), label.map(String::from), function);

        let fr = Trusted::new(self);
        let gen_id = self.generation_id.get();

        let global = self.global();
        let canceller = global.task_canceller(TaskSourceName::FileReading);
        let task_source = global.file_reading_task_source();

        thread::Builder::new()
            .name("file reader async operation".to_owned())
            .spawn(move || {
                perform_annotated_read_operation(
                    gen_id,
                    load_data,
                    blob_contents,
                    fr,
                    task_source,
                    canceller,
                )
            })
            .expect("Thread spawning failed");

        Ok(())
    }

    fn change_ready_state(&self, state: FileReaderReadyState) {
        self.ready_state.set(state);
    }
}

// https://w3c.github.io/FileAPI/#thread-read-operation
fn perform_annotated_read_operation(
    gen_id: GenerationId,
    data: ReadMetaData,
    blob_contents: Arc<Vec<u8>>,
    filereader: TrustedFileReader,
    task_source: FileReadingTaskSource,
    canceller: TaskCanceller,
) {
    // Step 4
    let task = FileReadingTask::ProcessRead(filereader.clone(), gen_id);
    task_source.queue_with_canceller(task, &canceller).unwrap();

    let task = FileReadingTask::ProcessReadData(filereader.clone(), gen_id);
    task_source.queue_with_canceller(task, &canceller).unwrap();

    let task = FileReadingTask::ProcessReadEOF(filereader, gen_id, data, blob_contents);
    task_source.queue_with_canceller(task, &canceller).unwrap();
}
