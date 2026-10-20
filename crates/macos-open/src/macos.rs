//! The sole native bridge: an NSObject receiver retained for the registration's lifetime.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::{DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationWillFinishLaunchingNotification};
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol,
};

use super::OpenRequest;

const EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const LIST: u32 = u32::from_be_bytes(*b"list");
const MAX_DOCUMENTS: usize = 4096;
const MAX_REQUESTS: usize = 64;
const MAX_PATH_UNITS: usize = 32768;
const MAX_REQUEST_BYTES: usize = 1 << 20;
const MAX_PENDING_BYTES: usize = 4 << 20;
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Pending {
    requests: VecDeque<OpenRequest>,
    documents: usize,
    path_bytes: usize,
    overflow: bool,
}

struct Queue {
    pending: Mutex<Pending>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Queue {
    fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self { pending: Mutex::new(Pending::default()), wake: Box::new(wake) }
    }

    fn push(&self, request: OpenRequest) {
        let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let documents = request.as_ref().map_or(0, Vec::len);
        let path_bytes = request.as_ref().map_or(0, |paths| paths.iter().fold(0usize, |sum, path| sum.saturating_add(path.as_os_str().len())));
        if pending.requests.len() >= MAX_REQUESTS
            || pending.documents.saturating_add(documents) > MAX_DOCUMENTS
            || pending.path_bytes.saturating_add(path_bytes) > MAX_PENDING_BYTES
        {
            pending.overflow = true;
        } else {
            pending.documents = pending.documents.saturating_add(documents);
            pending.path_bytes = pending.path_bytes.saturating_add(path_bytes);
            pending.requests.push_back(request);
        }
    }

    fn drain(&self) -> Vec<OpenRequest> {
        let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut requests: Vec<_> = pending.requests.drain(..).collect();
        pending.documents = 0;
        pending.path_bytes = 0;
        if std::mem::take(&mut pending.overflow) {
            requests.push(Err(
                "Too many Open Documents requests are waiting. Let the current import finish, then open the remaining photos again.".into()
            ));
        }
        requests
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. Ivars are owned Rust values and this
    // class does not implement Drop. Its callback touches only its own synchronized queue.
    #[unsafe(super(NSObject))]
    #[name = "LightCraftOpenDocumentsHandler"]
    #[ivars = Arc<Queue>]
    struct Handler;

    // SAFETY: NSObjectProtocol adds no safety requirements.
    unsafe impl NSObjectProtocol for Handler {}

    impl Handler {
        // SAFETY: NSAppleEventManager's selector contract is a void method taking two nullable
        // NSAppleEventDescriptor pointers. The registered selector below uses this exact ABI.
        #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
        fn receive(&self, event: Option<&NSAppleEventDescriptor>, _reply: Option<&NSAppleEventDescriptor>) {
            let request = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(event)))
                .unwrap_or_else(|_| Err("The macOS Open Documents request could not be read. Open the photos again with File > Import Photos.".into()));
            self.ivars().push(request);
            // A client callback must never unwind through Objective-C's event dispatch stack.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.ivars().wake)())).is_err() {
                self.ivars().push(Err("LightCraft could not refresh after receiving photos from macOS. Bring its window to the front to continue.".into()));
            }
        }

        // SAFETY: NSNotificationCenter's selector contract is a void method taking one
        // notification pointer; the registration below uses exactly this ABI.
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: Option<&NSNotification>) {
            self.register();
        }
    }
);

impl Handler {
    fn register(&self) {
        // SAFETY: This class implements the selector with NSAppleEventManager's documented
        // two-descriptor ABI. The owning Receiver retains it until after unregistration.
        unsafe {
            NSAppleEventManager::sharedAppleEventManager().setEventHandler_andSelector_forEventClass_andEventID(
                self,
                sel!(handleOpenDocuments:withReplyEvent:),
                EVENT_CLASS,
                OPEN_DOCUMENTS,
            );
        }
    }

    fn new(queue: Arc<Queue>) -> Retained<Self> {
        use objc2::AnyThread;
        let this = Self::alloc().set_ivars(queue);
        // SAFETY: NSObject's init initializes the allocated subclass and does not access or
        // replace its Rust ivars; the returned object retains this allocation's ownership.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct Receiver {
    handler: Retained<Handler>,
    manager: Retained<NSAppleEventManager>,
    notifications: Retained<NSNotificationCenter>,
    // Makes Receiver !Send/!Sync, so registration and unregistration stay on the main thread.
    _main_thread: MainThreadMarker,
}

impl Receiver {
    pub(super) fn install(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("Open Documents must be installed on the macOS main thread")?;
        if INSTALLED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err("The macOS Open Documents handler is already installed".into());
        }
        // Winit retains its own application delegate; replacing it would break its loop.
        let _app = NSApplication::sharedApplication(mtm);
        let manager = NSAppleEventManager::sharedAppleEventManager();
        let handler = Handler::new(Arc::new(Queue::new(wake)));
        let notifications = NSNotificationCenter::defaultCenter();
        // AppKit installs its default Apple Event handlers during finishLaunching, after
        // sharedApplication has returned. Register again at WillFinishLaunching, before the
        // initial Open Documents event is delivered, so cold launches retain our receiver.
        // SAFETY: Handler implements the exact one-notification ABI of this selector, is
        // retained by Receiver, and is removed as an observer before that retention ends.
        unsafe {
            notifications.addObserver_selector_name_object(
                &handler,
                sel!(applicationWillFinishLaunching:),
                Some(NSApplicationWillFinishLaunchingNotification),
                Some(&_app),
            );
        }
        // Also covers installation after AppKit has already completed launching.
        handler.register();
        Ok(Self { handler, manager, notifications, _main_thread: mtm })
    }

    pub(super) fn drain(&self) -> Vec<OpenRequest> {
        self.handler.ivars().drain()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: This is the live observer registered by install; removal does not retain or
        // call it, and no callback can race because Receiver is confined to the main thread.
        unsafe { self.notifications.removeObserver(&self.handler) };
        self.manager.removeEventHandlerForEventClass_andEventID(EVENT_CLASS, OPEN_DOCUMENTS);
        INSTALLED.store(false, Ordering::Release);
    }
}

fn decode(event: Option<&NSAppleEventDescriptor>) -> OpenRequest {
    let event = event.ok_or("macOS supplied an empty Open Documents event")?;
    let documents = event.paramDescriptorForKeyword(DIRECT_OBJECT).ok_or("The macOS Open Documents request has no document list")?;
    if documents.descriptorType() != LIST {
        return Err("The macOS Open Documents request must contain a list of files".into());
    }
    let count = usize::try_from(documents.numberOfItems()).map_err(|_| "The macOS Open Documents file count is invalid")?;
    if count == 0 || count > MAX_DOCUMENTS {
        return Err(format!("Open Documents accepts between 1 and {MAX_DOCUMENTS} files per request; macOS supplied {count}"));
    }
    let mut paths = Vec::with_capacity(count);
    let mut path_bytes = 0usize;
    for index in 1..=count {
        let index = isize::try_from(index).map_err(|_| "The macOS document index is too large")?;
        let item = documents.descriptorAtIndex(index).ok_or("A file is missing from the macOS Open Documents request")?;
        let url =
            item.fileURLValue().filter(|url| url.isFileURL()).ok_or("The macOS Open Documents request contains an invalid file URL or alias")?;
        let path = url.path().ok_or("The macOS Open Documents file URL has no path")?;
        if path.length() > MAX_PATH_UNITS {
            return Err("The macOS Open Documents file path is too long".into());
        }
        let path = PathBuf::from(path.to_string());
        if !path.is_absolute() {
            return Err("The macOS Open Documents request contains a relative or empty file path".into());
        }
        path_bytes = path_bytes.saturating_add(path.as_os_str().len());
        if path_bytes > MAX_REQUEST_BYTES {
            return Err("The macOS Open Documents request is too large. Open fewer photos at a time.".into());
        }
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests;
