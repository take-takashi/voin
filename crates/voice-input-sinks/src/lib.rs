use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::Mutex;

use arboard::Clipboard;
use voice_input_core::{
    OutputContext, OutputFormat, ProcessedText, SendReceipt, SinkError, TextSink,
};

/// 処理済みテキストを標準出力へ送るSinkです。
pub struct StdoutSink<W = io::Stdout> {
    writer: Mutex<W>,
    format: OutputFormat,
}

impl StdoutSink<io::Stdout> {
    pub fn new(format: OutputFormat) -> Self {
        Self::with_writer(format, io::stdout())
    }
}

impl<W: Write + Send> StdoutSink<W> {
    pub fn with_writer(format: OutputFormat, writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
            format,
        }
    }

    pub fn into_writer(self) -> Result<W, SinkError> {
        self.writer
            .into_inner()
            .map_err(|_| SinkError::new("stdout writer mutex is poisoned"))
    }

    fn render(&self, text: &ProcessedText, context: &OutputContext) -> String {
        match self.format {
            OutputFormat::Plain => format!("{}\n", text.text),
            OutputFormat::Json => {
                let language = text
                    .source
                    .language
                    .as_deref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned());

                format!(
                    "{{\"session_id\":{},\"text\":{},\"language\":{},\"duration_ms\":{}}}\n",
                    json_string(&context.session_id),
                    json_string(&text.text),
                    language,
                    text.source.duration.as_millis()
                )
            }
        }
    }
}

impl<W: Write + Send> TextSink for StdoutSink<W> {
    fn send(
        &self,
        text: &ProcessedText,
        context: &OutputContext,
    ) -> Result<SendReceipt, SinkError> {
        let output = self.render(text, context);
        let bytes_sent = output.len();
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| SinkError::new("stdout writer mutex is poisoned"))?;

        writer
            .write_all(output.as_bytes())
            .map_err(|error| SinkError::new(format!("failed to write stdout: {error}")))?;
        writer
            .flush()
            .map_err(|error| SinkError::new(format!("failed to flush stdout: {error}")))?;

        Ok(SendReceipt {
            sink_name: "stdout".to_owned(),
            bytes_sent,
            pasted: false,
        })
    }
}

/// 処理済みテキストをOSのクリップボードへコピーするSinkです。
pub struct ClipboardSink {
    clipboard: Mutex<Clipboard>,
}

impl ClipboardSink {
    pub fn new() -> Result<Self, SinkError> {
        let clipboard = Clipboard::new()
            .map_err(|error| SinkError::new(format!("failed to open clipboard: {error}")))?;

        Ok(Self {
            clipboard: Mutex::new(clipboard),
        })
    }
}

impl TextSink for ClipboardSink {
    fn send(
        &self,
        text: &ProcessedText,
        _context: &OutputContext,
    ) -> Result<SendReceipt, SinkError> {
        let bytes_sent = text.text.len();
        let mut clipboard = self
            .clipboard
            .lock()
            .map_err(|_| SinkError::new("clipboard mutex is poisoned"))?;

        clipboard
            .set_text(&text.text)
            .map_err(|error| SinkError::new(format!("failed to write clipboard: {error}")))?;

        Ok(SendReceipt {
            sink_name: "clipboard".to_owned(),
            bytes_sent,
            pasted: false,
        })
    }
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');

    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                let _ = write!(output, "\\u{:04x}", character as u32);
            }
            character => output.push(character),
        }
    }

    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::StdoutSink;
    use std::time::Duration;
    use voice_input_core::{OutputContext, OutputFormat, ProcessedText, TextSink, Transcript};

    fn sample_text() -> ProcessedText {
        ProcessedText {
            text: "hello \"voin\"".to_owned(),
            source: Transcript {
                text: "hello".to_owned(),
                language: Some("en".to_owned()),
                duration: Duration::from_millis(123),
                segments: Vec::new(),
            },
        }
    }

    #[test]
    fn writes_plain_text_with_a_trailing_newline() {
        let sink = StdoutSink::with_writer(OutputFormat::Plain, Vec::new());
        sink.send(&sample_text(), &OutputContext::new("session-1"))
            .expect("plain output must succeed");

        assert_eq!(
            sink.into_writer().expect("writer must be available"),
            b"hello \"voin\"\n"
        );
    }

    #[test]
    fn escapes_json_output() {
        let sink = StdoutSink::with_writer(OutputFormat::Json, Vec::new());
        sink.send(&sample_text(), &OutputContext::new("session-1"))
            .expect("json output must succeed");

        let output = String::from_utf8(sink.into_writer().unwrap()).unwrap();
        assert!(output.contains("\\\"voin\\\""));
        assert!(output.contains("\"duration_ms\":123"));
    }
}
