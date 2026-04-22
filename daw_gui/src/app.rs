use vizia::prelude::*;

#[derive(Default)]
pub struct AppData;

impl Model for AppData {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| {
            if let WindowEvent::WindowClose = window_event {
                tracing::info!("window close requested");
            }
        });
    }
}
