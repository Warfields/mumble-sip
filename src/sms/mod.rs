use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// A text message flowing through the SMS system.
pub struct SmsMessage {
    /// Phone number of the external party (E.164 digits, no '+').
    pub phone_number: String,
    /// Message body (plain text).
    pub body: String,
}

/// Receives inbound text messages from an external source.
///
/// Each backend (SIP MESSAGE, webhook, etc.) implements this trait to produce
/// a stream of incoming messages. Consumers pull from the returned channel
/// in the main event loop.
#[async_trait]
pub trait SmsReceiver: Send + Sync {
    /// Start receiving messages. Returns a channel of inbound messages.
    /// The receiver should be driven in the main event loop (or a spawned task).
    async fn start(&self) -> Result<mpsc::UnboundedReceiver<SmsMessage>>;
}

/// Sends outbound text messages to an external party.
///
/// Each backend implements the actual delivery mechanism (SIP MESSAGE,
/// provider REST API, etc.).
#[async_trait]
pub trait SmsSender: Send + Sync {
    /// Send a text message to the given phone number.
    async fn send(&self, message: &SmsMessage) -> Result<()>;
}
