use super::{SharedState, Tab, View};
use crate::shared_state::Event;

pub(crate) struct Connection;

impl Tab for Connection {
    fn new() -> Self {
        Self {}
    }

    fn name(&self) -> &'static str {
        "Connection"
    }
}

impl View for Connection {
    fn ui(&mut self, ui: &mut egui::Ui, state: &mut SharedState) {
        ui.add_enabled_ui(!state.is_listening, |ui| {
            ui.label("Listener Address");
            ui.text_edit_singleline(&mut state.listener_addr);
            ui.label("Server Address");
            ui.text_edit_singleline(&mut state.server_addr);
            ui.checkbox(&mut state.save_captures, "Save captures");
            ui.label("Capture Directory");
            ui.text_edit_singleline(&mut state.capture_dir);
        });

        ui.horizontal(|ui| {
            if state.is_listening {
                if ui.button("Stop Listening").clicked() {
                    state.send_event(Event::StopListening);
                }
            } else if ui.button("Start Listening").clicked() {
                state.send_event(Event::StartListening);
            }

            ui.checkbox(&mut state.autostart, "Autostart");
        });
    }
}
