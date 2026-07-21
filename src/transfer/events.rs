use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

pub type TransferId = String;
pub type OfferId = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfferDisposition {
    Accept,
    Reject,
    Pending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferState {
    Offered,
    Hashing,
    Negotiating,
    Transferring,
    Paused,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferEvent {
    pub transfer_id: TransferId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub file_name: String,
    pub durable_bytes: u64,
    pub in_flight_bytes: u64,
    pub total_bytes: u64,
    pub active_workers: u16,
    pub target_workers: u16,
    pub speed_bytes_per_second: f64,
    pub elapsed_ms: u64,
    pub error_code: Option<String>,
    pub message: Option<String>,
}

impl TransferEvent {
    pub fn new(
        transfer_id: TransferId,
        direction: TransferDirection,
        state: TransferState,
        file_name: String,
        total_bytes: u64,
        elapsed: Duration,
    ) -> Self {
        Self {
            transfer_id,
            direction,
            state,
            file_name,
            durable_bytes: 0,
            in_flight_bytes: 0,
            total_bytes,
            active_workers: 0,
            target_workers: 0,
            speed_bytes_per_second: 0.0,
            elapsed_ms: elapsed.as_millis() as u64,
            error_code: None,
            message: None,
        }
    }
}

pub trait TransferCallback: Send + Sync {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool;
    fn on_progress(&self, transferred: u64, total: u64);
    fn on_complete(&self, success: bool, msg: String);

    fn on_offer(
        &self,
        offer_id: OfferId,
        file_name: String,
        file_size: u64,
        sender_ip: String,
    ) -> OfferDisposition {
        let _ = offer_id;
        if self.on_receive_request(file_name, file_size, sender_ip) {
            OfferDisposition::Accept
        } else {
            OfferDisposition::Reject
        }
    }

    fn on_event(&self, event: TransferEvent) {
        match event.state {
            TransferState::Transferring | TransferState::Paused | TransferState::Verifying => {
                self.on_progress(event.durable_bytes, event.total_bytes);
            }
            TransferState::Completed => {
                self.on_progress(event.total_bytes, event.total_bytes);
                self.on_complete(
                    true,
                    event.message.unwrap_or_else(|| event.file_name.clone()),
                );
            }
            TransferState::Failed | TransferState::Cancelled => {
                self.on_complete(
                    false,
                    event.message.unwrap_or_else(|| "transfer failed".into()),
                );
            }
            _ => {}
        }
    }
}

pub type EventSubscriber = Arc<dyn Fn(TransferEvent) + Send + Sync>;
