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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferSegment {
    pub start: u64,
    pub end: u64,
    pub downloaded_end: u64,
    pub running: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferDiagnostics {
    pub hashing_ms: u64,
    pub negotiating_ms: u64,
    pub transferring_ms: u64,
    pub verifying_ms: u64,
    pub wire_bytes: u64,
    pub retransmitted_bytes: u64,
    pub split_trials: u32,
    pub rollbacks: u32,
    pub retries: u32,
    pub timeouts_or_resets: u32,
}

impl TransferDiagnostics {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
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
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub allocated_workers: u16,
    pub target_workers: u16,
    pub speed_bytes_per_second: f64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stage_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stage_total_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<TransferSegment>,
    #[serde(default, skip_serializing_if = "TransferDiagnostics::is_empty")]
    pub diagnostics: TransferDiagnostics,
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
            allocated_workers: 0,
            target_workers: 0,
            speed_bytes_per_second: 0.0,
            elapsed_ms: elapsed.as_millis() as u64,
            stage_bytes: 0,
            stage_total_bytes: 0,
            segments: Vec::new(),
            diagnostics: TransferDiagnostics::default(),
            error_code: None,
            message: None,
        }
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u16(value: &u16) -> bool {
    *value == 0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_segments_serialize_with_android_field_names() {
        let mut event = TransferEvent::new(
            "transfer".into(),
            TransferDirection::Receive,
            TransferState::Transferring,
            "file.bin".into(),
            1_000,
            Duration::from_millis(200),
        );
        event.segments.push(TransferSegment {
            start: 500,
            end: 1_000,
            downloaded_end: 750,
            running: true,
        });
        event.diagnostics = TransferDiagnostics {
            hashing_ms: 120,
            wire_bytes: 750,
            split_trials: 2,
            ..TransferDiagnostics::default()
        };

        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["segments"][0]["downloadedEnd"], 750);
        assert_eq!(json["segments"][0]["running"], true);
        assert_eq!(json["diagnostics"]["hashingMs"], 120);
        assert_eq!(json["diagnostics"]["wireBytes"], 750);
        assert_eq!(json["diagnostics"]["splitTrials"], 2);
    }

    #[test]
    fn legacy_events_without_segments_still_deserialize() {
        let event: TransferEvent = serde_json::from_value(serde_json::json!({
            "transferId": "legacy",
            "direction": "receive",
            "state": "transferring",
            "fileName": "file.bin",
            "durableBytes": 0,
            "inFlightBytes": 0,
            "totalBytes": 1,
            "activeWorkers": 0,
            "targetWorkers": 8,
            "speedBytesPerSecond": 0.0,
            "elapsedMs": 0,
            "errorCode": null,
            "message": null
        }))
        .unwrap();

        assert!(event.segments.is_empty());
        assert_eq!(event.diagnostics, TransferDiagnostics::default());
    }
}
