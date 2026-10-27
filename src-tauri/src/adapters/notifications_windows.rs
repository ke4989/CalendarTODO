use crate::{
    adapters::persistence::NotificationJob,
    application::reminders::{DeliveryFailure, ReminderDelivery},
};
use tauri::Manager;
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    core::{HSTRING, IInspectable},
};

/// Windows-only delivery; the toast carries only a local opaque job ID.
/// No title, notes, URL, UID, credential, or raw ICS reaches Windows XML.
pub struct WindowsToastDelivery(pub tauri::AppHandle);

impl ReminderDelivery for WindowsToastDelivery {
    fn deliver(&self, job: &NotificationJob) -> Result<(), DeliveryFailure> {
        let xml = XmlDocument::new().map_err(|_| DeliveryFailure::Unavailable)?;
        xml.LoadXml(&HSTRING::from(
            r#"<toast><visual><binding template="ToastGeneric"><text>Calendar TODO</text><text>You have a reminder</text></binding></visual></toast>"#,
        ))
        .map_err(|_| DeliveryFailure::Unavailable)?;
        let toast = ToastNotification::CreateToastNotification(&xml)
            .map_err(|_| DeliveryFailure::Unavailable)?;
        toast
            .SetTag(&HSTRING::from(&job.job_id))
            .map_err(|_| DeliveryFailure::Unavailable)?;
        toast
            .SetGroup(&HSTRING::from("calendar-todo"))
            .map_err(|_| DeliveryFailure::Unavailable)?;
        let app = self.0.clone();
        let activated = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            Ok(())
        });
        toast
            .Activated(&activated)
            .map_err(|_| DeliveryFailure::Unavailable)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
            &self.0.config().identifier,
        ))
        .and_then(|notifier| notifier.Show(&toast))
        .map_err(|_| DeliveryFailure::Unavailable)
    }
}
