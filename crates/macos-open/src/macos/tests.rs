use std::sync::atomic::{AtomicUsize, Ordering};

use objc2_foundation::{NSString, NSURL};

use super::*;

fn event(list: Option<&NSAppleEventDescriptor>) -> Retained<NSAppleEventDescriptor> {
    let event =
        NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(EVENT_CLASS, OPEN_DOCUMENTS, None, -1, 0);
    if let Some(list) = list {
        event.setParamDescriptor_forKeyword(list, DIRECT_OBJECT);
    }
    event
}

fn file_list(paths: &[&str]) -> Retained<NSAppleEventDescriptor> {
    let list = NSAppleEventDescriptor::listDescriptor();
    for path in paths {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        list.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithFileURL(&url), 0);
    }
    list
}

/// This is the Objective-C selector registered with NSAppleEventManager, not a separate test
/// parser: before this bridge existed the native event had no receiver and no paths arrived.
#[test]
fn native_handler_queues_raf_batches_and_wakes_the_application() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = wakes.clone();
    let queue = Arc::new(Queue::new(move || {
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    let handler = Handler::new(queue.clone());
    // macOS file URLs use decomposed Unicode (as its filesystem does).
    let paths = ["/tmp/Photos External Editors/富士 film%20.RAF", "/tmp/Cafe\u{301} #1.raf"];
    let event = event(Some(&file_list(&paths)));
    let reply = NSAppleEventDescriptor::nullDescriptor();
    // SAFETY: This invokes the actual registered selector on our concrete Handler, with live
    // descriptors matching its declared nullable-reference arguments and void return type.
    unsafe {
        let _: () = msg_send![&*handler, handleOpenDocuments: &*event, withReplyEvent: &*reply];
    }
    assert_eq!(queue.drain(), vec![Ok(paths.iter().map(PathBuf::from).collect())]);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(queue.drain().is_empty());
}

#[test]
fn file_url_escapes_are_decoded_once() {
    let list = NSAppleEventDescriptor::listDescriptor();
    let url = NSURL::URLWithString(&NSString::from_str("file:///tmp/Fuji%20%E5%AF%8C%E5%A3%AB%2520%23.RAF")).unwrap();
    list.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithFileURL(&url), 0);
    assert_eq!(decode(Some(&event(Some(&list)))), Ok(vec![PathBuf::from("/tmp/Fuji 富士%20#.RAF")]));
}

#[test]
fn requests_survive_until_a_ui_context_can_be_attached() {
    let attached = Arc::new(AtomicBool::new(false));
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed_attached = attached.clone();
    let observed_wakes = wakes.clone();
    let queue = Arc::new(Queue::new(move || {
        if observed_attached.load(Ordering::Relaxed) {
            observed_wakes.fetch_add(1, Ordering::Relaxed);
        }
    }));
    let handler = Handler::new(queue.clone());
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&file_list(&["/tmp/first.raf"])))), None);
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    attached.store(true, Ordering::Relaxed);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&file_list(&["/tmp/second.tiff"])))), None);
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert_eq!(queue.drain(), vec![Ok(vec![PathBuf::from("/tmp/first.raf")]), Ok(vec![PathBuf::from("/tmp/second.tiff")])]);
}

#[test]
fn malformed_native_events_become_errors_and_do_not_poison_later_opens() {
    let queue = Arc::new(Queue::new(|| {}));
    let handler = Handler::new(queue.clone());
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), None, None);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(None)), None);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&NSAppleEventDescriptor::descriptorWithInt32(4)))), None);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&NSAppleEventDescriptor::listDescriptor()))), None);
    let bad = NSAppleEventDescriptor::listDescriptor();
    bad.insertDescriptor_atIndex(&NSAppleEventDescriptor::descriptorWithInt32(42), 0);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&bad))), None);
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&file_list(&["/tmp/valid.raf"])))), None);
    let requests = queue.drain();
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().take(5).all(Result::is_err));
    assert_eq!(requests.last(), Some(&Ok(vec![PathBuf::from("/tmp/valid.raf")])));
}

#[test]
fn oversized_native_batches_and_pending_requests_are_bounded() {
    let list = NSAppleEventDescriptor::listDescriptor();
    let url = NSURL::fileURLWithPath(&NSString::from_str("/tmp/a.raf"));
    let file = NSAppleEventDescriptor::descriptorWithFileURL(&url);
    for _ in 0..=MAX_DOCUMENTS {
        list.insertDescriptor_atIndex(&file, 0);
    }
    assert!(decode(Some(&event(Some(&list)))).is_err());
    let queue = Queue::new(|| {});
    for _ in 0..=MAX_REQUESTS {
        queue.push(Ok(vec![PathBuf::from("/tmp/a.raf")]));
    }
    let requests = queue.drain();
    assert_eq!(requests.len(), MAX_REQUESTS + 1);
    assert!(requests.last().is_some_and(Result::is_err));
    assert!(queue.drain().is_empty());
    queue.push(Ok(vec![PathBuf::from("/tmp/b.raf"); MAX_DOCUMENTS]));
    queue.push(Ok(vec![PathBuf::from("/tmp/c.raf")]));
    assert_eq!(queue.drain().len(), 2);
}

#[test]
fn native_path_and_aggregate_payload_sizes_are_bounded() {
    let too_long = format!("/tmp/{}.raf", "a".repeat(MAX_PATH_UNITS));
    assert!(decode(Some(&event(Some(&file_list(&[&too_long]))))).is_err());
    let long = format!("/tmp/{}.raf", "a".repeat(MAX_PATH_UNITS - 16));
    let paths = vec![long.as_str(); MAX_REQUEST_BYTES / long.len() + 1];
    assert!(decode(Some(&event(Some(&file_list(&paths))))).is_err());
    let queue = Queue::new(|| {});
    let large = PathBuf::from(format!("/{}", "a".repeat(MAX_REQUEST_BYTES - 1)));
    for _ in 0..MAX_PENDING_BYTES / MAX_REQUEST_BYTES {
        queue.push(Ok(vec![large.clone()]));
    }
    queue.push(Ok(vec![PathBuf::from("/tmp/one-too-many.raf")]));
    let requests = queue.drain();
    assert_eq!(requests.len(), MAX_PENDING_BYTES / MAX_REQUEST_BYTES + 1);
    assert!(requests.last().is_some_and(Result::is_err));
    queue.push(Ok(vec![PathBuf::from("/tmp/after-drain.raf")]));
    assert_eq!(queue.drain(), vec![Ok(vec![PathBuf::from("/tmp/after-drain.raf")])]);
}

#[test]
fn a_panicking_waker_cannot_unwind_into_objc() {
    let queue = Arc::new(Queue::new(|| panic!("client callback failed")));
    let handler = Handler::new(queue.clone());
    handler.receive(sel!(handleOpenDocuments:withReplyEvent:), Some(&event(Some(&file_list(&["/tmp/valid.raf"])))), None);
    let requests = queue.drain();
    assert_eq!(requests.first(), Some(&Ok(vec![PathBuf::from("/tmp/valid.raf")])));
    assert!(requests.last().is_some_and(Result::is_err));
}
