// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{models::execd::ExecutionEvent, Error, Result};
use futures_core::Stream;
use futures_util::StreamExt;
use std::pin::Pin;

pub type ExecutionStream = Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Owns the response body. Dropping this stream releases it, without interrupting
/// the remote process. Call Commands::interrupt explicitly when desired.
pub(crate) fn events(response: reqwest::Response) -> ExecutionStream {
    json_events(response)
}

pub(crate) fn json_events<T: serde::de::DeserializeOwned + Send + 'static>(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<T>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let mut body = response.bytes_stream();
        let mut decoder = Decoder::<T>::default();
        while let Some(chunk) = body.next().await {
            for event in decoder.push(&chunk.map_err(|error| Error::Transport(error.without_url()))?, false)? { yield event; }
        }
        for event in decoder.push(&[], true)? { yield event; }
    })
}

struct Decoder<T = ExecutionEvent> {
    pending: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    marker: std::marker::PhantomData<T>,
}
impl<T> Default for Decoder<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            data: Vec::new(),
            data_bytes: 0,
            marker: std::marker::PhantomData,
        }
    }
}
impl<T: serde::de::DeserializeOwned> Decoder<T> {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<T>> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut consumed = 0;
        while let Some(relative) = self.pending[consumed..]
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')
        {
            let index = consumed + relative;
            // CRLF may be fragmented across network reads.
            if self.pending[index] == b'\r' && index + 1 == self.pending.len() && !eof {
                break;
            }
            let line = std::str::from_utf8(&self.pending[consumed..index])
                .map_err(|e| Error::Protocol(format!("Invalid UTF-8 in execution stream: {e}")))?
                .to_owned();
            consumed = index + 1;
            if self.pending[index] == b'\r' && self.pending.get(consumed) == Some(&b'\n') {
                consumed += 1;
            }
            self.line(&line, &mut output)?;
        }
        self.pending.drain(..consumed);
        if self.pending.len() + self.data_bytes > MAX_FRAME_BYTES {
            return Err(Error::Protocol(
                "Execution stream frame exceeds 16 MiB".into(),
            ));
        }
        if eof {
            if !self.pending.is_empty() {
                let line = std::str::from_utf8(&self.pending)
                    .map_err(|e| {
                        Error::Protocol(format!("Invalid UTF-8 in execution stream: {e}"))
                    })?
                    .to_owned();
                self.pending.clear();
                self.line(&line, &mut output)?;
            }
            self.flush(&mut output)?;
        }
        Ok(output)
    }
    fn line(&mut self, line: &str, output: &mut Vec<T>) -> Result<()> {
        if line.is_empty() {
            return self.flush(output);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            self.data_bytes += data.len() + 1;
            if self.data_bytes > MAX_FRAME_BYTES {
                return Err(Error::Protocol(
                    "Execution stream frame exceeds 16 MiB".into(),
                ));
            }
            self.data.push(data.to_owned());
        } else if line.trim_start().starts_with('{') {
            // JSON-lines servers do not emit blank frame delimiters.
            self.flush(output)?;
            output.push(serde_json::from_str(line)?);
        }
        // event/id/retry and unknown SSE fields are ignored per SSE semantics.
        Ok(())
    }
    fn flush(&mut self, output: &mut Vec<T>) -> Result<()> {
        if !self.data.is_empty() {
            let data = self.data.join("\n");
            self.data.clear();
            self.data_bytes = 0;
            if !data.trim().is_empty() {
                output.push(serde_json::from_str(&data)?);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_utf8_crlf_and_multidata() {
        let input = ": comment\r\nevent: stdout\r\ndata: {\r\ndata: \"type\":\"stdout\",\"text\":\"你好\"}\r\n\r\n".as_bytes();
        let mut parser = Decoder::<ExecutionEvent>::default();
        let mut events = Vec::new();
        for byte in input {
            events.extend(parser.push(&[*byte], false).unwrap());
        }
        events.extend(parser.push(&[], true).unwrap());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text.as_deref(), Some("你好"));
    }
    #[test]
    fn json_lines_and_eof_frame() {
        let mut parser = Decoder::<ExecutionEvent>::default();
        let events = parser
            .push(
                b"{\"type\":\"stdout\"}\ndata: {\"type\":\"execution_complete\"}",
                true,
            )
            .unwrap();
        assert_eq!(events.len(), 2);
    }
    #[test]
    fn malformed_json_and_utf8_are_errors() {
        assert!(Decoder::<ExecutionEvent>::default()
            .push(b"data: {bad}\n\n", true)
            .is_err());
        assert!(Decoder::<ExecutionEvent>::default()
            .push(&[0xff], true)
            .is_err());
    }
}
