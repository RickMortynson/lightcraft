//! Photos' Edit With and Finder's Open With use Apple Events rather than argv.
//! Native callbacks only enqueue paths. The ordinary background importer reads the files.

use std::sync::{Arc, Mutex};

use lightcraft_macos_open::{OpenDocuments, OpenRequest};
use lightcraft_ui_egui::LightcraftApp;

pub struct Documents {
    receiver: OpenDocuments,
    context: Arc<Mutex<Option<egui::Context>>>,
}

impl Documents {
    pub fn install() -> Result<Self, String> {
        let context: Arc<Mutex<Option<egui::Context>>> = Arc::default();
        let wake = context.clone();
        let receiver = OpenDocuments::install(move || {
            if let Some(ctx) = &*wake.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                ctx.request_repaint();
            }
        })?;
        Ok(Self { receiver, context })
    }

    pub fn attach(&self, ctx: &egui::Context) {
        *self.context.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        ctx.request_repaint();
    }

    pub fn poll(&self, app: &mut LightcraftApp, ctx: &egui::Context) {
        // Do not drain while busy: the bounded native queue keeps every accepted request for
        // the next import, including requests received before a failed library can be opened.
        if ready(app) {
            import_requests(app, ctx, self.receiver.drain());
        }
    }

    pub fn launch_imports(&self) -> (Vec<String>, Vec<String>) {
        requested_paths(self.receiver.drain())
    }
}

fn ready(app: &LightcraftApp) -> bool {
    app.import.is_none()
        && app.scan.is_none()
        && app.sync.is_none()
        && app.sync_run.is_none()
        && app.lightroom.is_none()
        && app.library_problem.as_ref().is_none_or(|problem| problem.dismissed)
}

fn requested_paths(requests: Vec<OpenRequest>) -> (Vec<String>, Vec<String>) {
    let mut paths = Vec::new();
    let mut notices = Vec::new();
    for request in requests {
        match request {
            Ok(files) => paths.extend(files.into_iter().map(|path| path.to_string_lossy().into_owned())),
            Err(error) => notices.push(error),
        }
    }
    if paths.is_empty() {
        return (paths, notices);
    }
    // Same root/home-folder protections as command-line imports; never walk arbitrary drives
    // in response to a native document request. Photo contents are read on the import worker.
    let (paths, refused) = super::launch_imports(&paths);
    notices.extend(refused);
    (paths, notices)
}

fn import_requests(app: &mut LightcraftApp, ctx: &egui::Context, requests: Vec<OpenRequest>) {
    let (paths, notices) = requested_paths(requests);
    app.notices.extend(notices);
    if paths.is_empty() {
        return;
    }
    if let Err(error) = lightcraft_ui_egui::import::start_paths(app, paths) {
        app.notices.push(format!("The photos received from macOS could not be imported: {error}"));
    } else {
        app.ui.view = lightcraft_ui_egui::state::ViewMode::PhotoGrid;
    }
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_ui_egui::{Services, panels::library_problem::LibraryProblem};

    #[test]
    fn native_requests_use_the_background_importer_and_wait_for_it_to_finish() {
        let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), Services::default());
        let ctx = egui::Context::default();
        assert!(ready(&app));
        import_requests(&mut app, &ctx, vec![Ok(vec!["/tmp/first photo.RAF".into()]), Ok(vec!["/tmp/second.tiff".into()])]);
        assert!(app.import.is_some());
        assert_eq!(app.import.as_ref().unwrap().status()["total"], 2);
        assert!(!ready(&app), "a second native request must stay queued instead of failing as busy");
        app.import = None;
        assert!(ready(&app));
        app.library_problem = Some(LibraryProblem::new("/tmp/library", "locked"));
        assert!(!ready(&app), "wait for a library choice");
        app.library_problem.as_mut().unwrap().dismissed = true;
        assert!(ready(&app), "Continue Without Saving allows importing");
    }

    #[test]
    fn malformed_native_requests_report_an_actionable_notice() {
        let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), Services::default());
        import_requests(&mut app, &egui::Context::default(), vec![Err("invalid file URL".into())]);
        assert_eq!(app.notices, ["invalid file URL"]);
        assert!(app.import.is_none());
    }
}
