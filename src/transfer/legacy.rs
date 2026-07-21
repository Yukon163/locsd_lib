//! Compatibility boundary for the fixed-chunk V2 and original text protocol.
//!
//! New code enters this module only after the peer explicitly reports V3 as
//! unsupported. The implementation remains isolated from the adaptive V3
//! scheduler so fixed chunk indexes can never be mistaken for arbitrary ranges.

use super::TransferCallback;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

pub(crate) fn handle_connection(
    socket: TcpStream,
    save_dir: Arc<String>,
    callback: Arc<dyn TransferCallback>,
    progress: Arc<Mutex<u64>>,
    total: Arc<Mutex<u64>>,
) {
    crate::core::handle_incoming_connection(socket, save_dir, callback, progress, total);
}

pub(crate) fn send_file(
    target_ip: &str,
    port: u16,
    file_path: &str,
    file_name: &str,
    file_len: u64,
    callback: Arc<dyn TransferCallback>,
) -> Result<(), String> {
    match crate::core::send_file_resume_inner(
        target_ip,
        port,
        file_path,
        file_name,
        file_len,
        8,
        callback.clone(),
    ) {
        Ok(true) => Ok(()),
        Ok(false) => crate::core::send_file_legacy_inner(
            target_ip, port, file_path, file_name, file_len, 8, callback,
        ),
        Err(error) => Err(format!("V2 compatibility negotiation failed: {error}")),
    }
}
