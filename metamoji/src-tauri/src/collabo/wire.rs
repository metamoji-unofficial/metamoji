//! The collabo socket's framing and parameter syntax.
//!
//! `NsCollaboSocket` speaks a line protocol of its own over raw TLS — not
//! WebSocket, not HTTP. One frame:
//!
//! ```text
//! \n{boothId}\t{packetNo}\t{paramString}\n[raw binary payload]
//! ```
//!
//! * the leading `\n` is a separator, so the stream begins with one
//! * the parameter line is newline-*terminated* as well: `postCommand` writes
//!   `"\n%s\t%s\t%s"` and every command format string already ends in `\n`
//!   (`NsCollaboSocket.smali` L2908 and the `cmd:…\n` literals below it)
//! * `boothId` is a logical channel; `*` for anything room-level
//! * `packetNo` is `C0`, `C1`, … from a counter shared across the process
//! * `paramString` is space-separated `key:value`, always led by `cmd:{name}`
//! * a payload follows the line with **no delimiter at all**, and only when
//!   the params say `binary:{N}` with N > 0
//!
//! That last rule is why this cannot be a line-based reader: after a header
//! announcing a payload, the next N bytes are opaque and may contain `\n`.
//!
//! Everything here is pure. The socket that carries it is in `socket.rs`.

use std::collections::BTreeMap;

use crate::error::{AppError, AppResult};

/// A frame in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub booth_id: String,
    pub packet_no: String,
    /// `cmd:` pulled out of the parameters, since every frame has one.
    pub command: String,
    pub params: BTreeMap<String, String>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(command: &str) -> Self {
        Self {
            booth_id: "*".to_string(),
            packet_no: String::new(),
            command: command.to_string(),
            params: BTreeMap::new(),
            payload: Vec::new(),
        }
    }

    pub fn booth(mut self, booth_id: &str) -> Self {
        self.booth_id = booth_id.to_string();
        self
    }

    pub fn param(mut self, key: &str, value: impl ToString) -> Self {
        self.params.insert(key.to_string(), value.to_string());
        self
    }

    pub fn payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.parse().ok()
    }

    /// Serialises the frame, payload and all.
    ///
    /// `binary:` is written by the caller through `payload()`, not asserted
    /// here, because the original sends text frames with no such key at all.
    pub fn encode(&self, packet_no: &str) -> Vec<u8> {
        let mut params = format!("cmd:{}", self.command);
        for (key, value) in &self.params {
            params.push(' ');
            params.push_str(key);
            params.push(':');
            params.push_str(value);
        }

        let mut out = Vec::with_capacity(params.len() + self.payload.len() + 16);
        out.push(b'\n');
        out.extend_from_slice(self.booth_id.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(packet_no.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(params.as_bytes());
        // Terminates the parameter line. A payload, if any, starts here.
        out.push(b'\n');
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Splits `key:value key:value …` on spaces, then each token on the *first*
/// colon.
///
/// Splitting on every colon would destroy any value containing one — a base64
/// blob, a timestamp — which is why `parseParameter` limits itself to two.
pub fn parse_params(text: &str) -> (String, BTreeMap<String, String>) {
    let mut command = String::new();
    let mut params = BTreeMap::new();

    for token in text.split(' ').filter(|t| !t.is_empty()) {
        let Some((key, value)) = token.split_once(':') else {
            continue;
        };
        if key == "cmd" {
            command = value.to_string();
        } else {
            params.insert(key.to_string(), value.to_string());
        }
    }
    (command, params)
}

/// Accumulates bytes and yields whole frames.
///
/// Stateful because a payload can straddle any number of reads, and because
/// the length that tells us where a payload ends is inside the header that
/// precedes it.
/// Neither accumulation below is allowed to grow past this. A real header
/// line is a handful of `key:value` tokens; a real edit payload is a
/// hand-drawn stroke or an image ticket, never hundreds of megabytes. A peer
/// (or the relay) that keeps sending bytes without a `\n`, or that announces
/// an absurd `binary:N`, is attempting memory exhaustion, not sending data.
const MAX_HEADER_LINE: usize = 64 * 1024;
const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// Set once a header announced `binary:N`; the next N bytes are payload.
    pending: Option<(Frame, usize)>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, `Ok(None)` if more bytes are needed, or
    /// `Err` if the peer has exceeded a size limit — the caller should treat
    /// that the same as any other unrecoverable connection error.
    pub fn next_frame(&mut self) -> AppResult<Option<Frame>> {
        if let Some((frame, size)) = self.pending.take() {
            if self.buffer.len() < size {
                self.pending = Some((frame, size));
                return Ok(None);
            }
            let payload = self.buffer.drain(..size).collect();
            return Ok(Some(frame.payload(payload)));
        }

        // Frames are separated by `\n`, so a leading one is noise.
        while self.buffer.first() == Some(&b'\n') {
            self.buffer.remove(0);
        }

        let Some(end) = self.buffer.iter().position(|b| *b == b'\n') else {
            if self.buffer.len() > MAX_HEADER_LINE {
                return Err(AppError::other("collabo: ヘッダ行が長すぎます"));
            }
            return Ok(None);
        };
        let line: Vec<u8> = self.buffer.drain(..end).collect();
        // Drop the terminator too, so a payload begins at offset 0.
        self.buffer.remove(0);
        // The server ends its header lines with CRLF. Left on, the carriage
        // return rides along on the *last* parameter's value, which is how
        // `status:0` arrives as `"0\r"` and compares equal to nothing.
        let line = String::from_utf8_lossy(&line);
        let line = line.strip_suffix('\r').unwrap_or(&line).to_string();

        let mut parts = line.splitn(3, '\t');
        let booth_id = parts.next().unwrap_or("*").to_string();
        let packet_no = parts.next().unwrap_or_default().to_string();
        let (command, params) = parse_params(parts.next().unwrap_or_default());

        let frame = Frame {
            booth_id,
            packet_no,
            command,
            params,
            payload: Vec::new(),
        };

        // `binary:N` means N raw bytes follow immediately, no separator.
        let size = frame
            .get("binary")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if size == 0 {
            return Ok(Some(frame));
        }
        if size > MAX_PAYLOAD_SIZE {
            return Err(AppError::other(format!(
                "collabo: binary:{size} が上限 {MAX_PAYLOAD_SIZE} を超えています"
            )));
        }

        if self.buffer.len() < size {
            self.pending = Some((frame, size));
            return Ok(None);
        }
        let payload = self.buffer.drain(..size).collect();
        Ok(Some(frame.payload(payload)))
    }
}

/// `C0`, `C1`, … — the sequence `sendPacketNo()` produces.
#[derive(Debug, Default)]
pub struct PacketCounter(u64);

impl PacketCounter {
    pub fn next(&mut self) -> String {
        let n = self.0;
        self.0 += 1;
        format!("C{n}")
    }
}

#[path = "wire/tests.rs"]
#[cfg(test)]
mod tests;
