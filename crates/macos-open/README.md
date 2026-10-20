# lightcraft-macos-open

An isolated safe bridge for macOS **Open Documents** (`aevt` / `odoc`) Apple Events,
used by Finder's Open With and Photos' Edit With. Winit 0.30 does not forward these
events to the application.

Call `OpenDocuments::install(waker)` on the main thread **before** starting the
window event loop. Keep the returned receiver alive until the event loop ends.
The waker may request a repaint once the UI context exists; events received before
then remain queued. AppKit installs its default file handlers during launch, so the
bridge registers again at WillFinishLaunching, before initial documents arrive. Drain the receiver from the application's normal frame logic
and hand paths to the usual import flow. No file is read in the native callback.
Dropping the receiver unregisters its handler. Only one receiver may be installed
per process. Other platforms return an unsupported error.

The private macOS module contains the Objective-C class declaration and handler
registration that require `unsafe`; callers receive only owned paths or actionable
errors. Incoming descriptor and queue sizes are bounded. The callback accepts both
file-URL and alias descriptors through Foundation's `fileURLValue`, preserving
Unicode and decoding URL escapes using Foundation rather than string slicing.
