use std::fmt::Display;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use voice_input_core::{
    AppError, CancellationToken, DeterministicPostProcessor, ProcessingContext, RecordingOptions,
    SendReceipt, SessionCoordinator, SessionState, TranscriptionOptions,
};
use voice_input_recorder_cpal::CpalRecorder;
use voice_input_sinks::ClipboardSink;
use voice_input_transcriber_whisper::WhisperTranscriber;

pub const DEFAULT_ENDPOINT: &str = "127.0.0.1:38741";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentCommand {
    Start,
    Stop,
    Toggle,
    Cancel,
    Status,
    Reset,
    Shutdown,
}

impl AgentCommand {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            "toggle" => Ok(Self::Toggle),
            "cancel" => Ok(Self::Cancel),
            "status" => Ok(Self::Status),
            "reset" => Ok(Self::Reset),
            "shutdown" => Ok(Self::Shutdown),
            value => Err(format!(
                "unknown agent command '{value}'; expected start, stop, toggle, cancel, status, reset, or shutdown"
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgentReply {
    State(SessionState),
    Completed(SendReceipt),
    Shutdown,
}

pub struct AgentRuntime {
    coordinator: SessionCoordinator,
    recording_options: RecordingOptions,
    transcription_options: TranscriptionOptions,
    processing_context: ProcessingContext,
    operation_cancel: CancellationToken,
}

impl AgentRuntime {
    pub fn new(
        coordinator: SessionCoordinator,
        recording_options: RecordingOptions,
        transcription_options: TranscriptionOptions,
        processing_context: ProcessingContext,
        operation_cancel: CancellationToken,
    ) -> Self {
        Self {
            coordinator,
            recording_options,
            transcription_options,
            processing_context,
            operation_cancel,
        }
    }

    pub fn state(&self) -> SessionState {
        self.coordinator.state()
    }

    pub fn handle(&mut self, command: AgentCommand) -> Result<AgentReply, AppError> {
        match command {
            AgentCommand::Start => {
                self.coordinator.start_recording(&self.recording_options)?;
                Ok(AgentReply::State(self.coordinator.state()))
            }
            AgentCommand::Stop => self.stop_and_send(),
            AgentCommand::Toggle => match self.coordinator.state() {
                SessionState::Idle => {
                    self.coordinator.start_recording(&self.recording_options)?;
                    Ok(AgentReply::State(self.coordinator.state()))
                }
                SessionState::Recording => self.stop_and_send(),
                SessionState::Failed => {
                    self.coordinator.reset()?;
                    self.coordinator.start_recording(&self.recording_options)?;
                    Ok(AgentReply::State(self.coordinator.state()))
                }
                actual => Err(AppError::InvalidState {
                    expected: SessionState::Idle,
                    actual,
                }),
            },
            AgentCommand::Cancel => {
                self.coordinator.cancel_recording()?;
                Ok(AgentReply::State(self.coordinator.state()))
            }
            AgentCommand::Status => Ok(AgentReply::State(self.coordinator.state())),
            AgentCommand::Reset => {
                self.coordinator.reset()?;
                Ok(AgentReply::State(self.coordinator.state()))
            }
            AgentCommand::Shutdown => Ok(AgentReply::Shutdown),
        }
    }

    fn stop_and_send(&mut self) -> Result<AgentReply, AppError> {
        let receipt = self.coordinator.stop_and_send(
            &self.transcription_options,
            &self.processing_context,
            &self.operation_cancel,
        )?;
        Ok(AgentReply::Completed(receipt))
    }
}

pub fn command(args: &[String]) -> Result<(), String> {
    let Some(action) = args.get(1).map(String::as_str) else {
        print_help();
        return Err("missing agent action".to_owned());
    };

    match action {
        "daemon" => daemon_command(args),
        "start" | "stop" | "toggle" | "cancel" | "status" | "reset" | "shutdown" => {
            client_command(action, args)
        }
        _ => {
            print_help();
            Err("unknown agent action".to_owned())
        }
    }
}

fn daemon_command(args: &[String]) -> Result<(), String> {
    let model = required_flag(args, "--model")?;
    let endpoint = flag_value(args, "--endpoint").unwrap_or(DEFAULT_ENDPOINT);
    let shutdown = install_shutdown_handler()?;
    let transcriber = WhisperTranscriber::from_model_path(model)
        .map_err(|error| format!("failed to initialize transcriber: {error}"))?;
    let sink = ClipboardSink::new()
        .map_err(|error| format!("failed to initialize clipboard sink: {error}"))?;
    let coordinator = SessionCoordinator::new(
        Box::new(CpalRecorder::new()),
        Box::new(transcriber),
        Box::new(DeterministicPostProcessor),
        Box::new(sink),
    );
    let mut runtime = AgentRuntime::new(
        coordinator,
        RecordingOptions::default(),
        TranscriptionOptions::default(),
        ProcessingContext::default(),
        shutdown.clone(),
    );

    run_server(endpoint, &mut runtime, &shutdown)
        .map_err(|error| format!("agent server failed: {error}"))
}

fn client_command(action: &str, args: &[String]) -> Result<(), String> {
    let endpoint = flag_value(args, "--endpoint").unwrap_or(DEFAULT_ENDPOINT);
    let command = AgentCommand::parse(action)?;
    let response = send_command(endpoint, command)?;
    println!("{response}");
    Ok(())
}

pub fn run_server(
    endpoint: &str,
    runtime: &mut AgentRuntime,
    shutdown: &CancellationToken,
) -> io::Result<()> {
    let listener = TcpListener::bind(endpoint)?;
    listener.set_nonblocking(true)?;
    eprintln!("voin agent listening on {endpoint}");

    while !shutdown.is_cancelled() {
        match listener.accept() {
            Ok((stream, _address)) => {
                let requested_shutdown = handle_connection(stream, runtime)?;
                if requested_shutdown {
                    shutdown.cancel();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }

    if runtime.state() == SessionState::Recording {
        let _ = runtime.handle(AgentCommand::Cancel);
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, runtime: &mut AgentRuntime) -> io::Result<bool> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        if reader.read_line(&mut line)? == 0 {
            return Ok(false);
        }
    }

    if line.len() > 128 {
        write_response(&mut stream, "ERR command is too long")?;
        return Ok(false);
    }

    let command = match AgentCommand::parse(&line) {
        Ok(command) => command,
        Err(error) => {
            write_response(&mut stream, &format!("ERR {error}"))?;
            return Ok(false);
        }
    };
    let requested_shutdown = command == AgentCommand::Shutdown;
    let response = match runtime.handle(command) {
        Ok(reply) => render_reply(reply),
        Err(error) => format!("ERR {}", sanitize_message(error)),
    };
    write_response(&mut stream, &response)?;
    Ok(requested_shutdown)
}

fn write_response(stream: &mut TcpStream, response: &str) -> io::Result<()> {
    stream.write_all(response.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn send_command(endpoint: &str, command: AgentCommand) -> Result<String, String> {
    let mut stream = TcpStream::connect(endpoint)
        .map_err(|error| format!("failed to connect to agent at {endpoint}: {error}"))?;
    stream
        .write_all(command_name(command).as_bytes())
        .map_err(|error| format!("failed to send agent command: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("failed to send agent command: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush agent command: {error}"))?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|error| format!("failed to read agent response: {error}"))?;
    let response = response.trim_end().to_owned();
    if let Some(error) = response.strip_prefix("ERR ") {
        return Err(error.to_owned());
    }
    Ok(response)
}

fn render_reply(reply: AgentReply) -> String {
    match reply {
        AgentReply::State(state) => format!("OK state={}", state_name(state)),
        AgentReply::Completed(receipt) => format!(
            "OK completed sink={} bytes={}",
            receipt.sink_name, receipt.bytes_sent
        ),
        AgentReply::Shutdown => "OK shutdown".to_owned(),
    }
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Recording => "recording",
        SessionState::Transcribing => "transcribing",
        SessionState::PostProcessing => "post_processing",
        SessionState::Sending => "sending",
        SessionState::Completed => "completed",
        SessionState::Failed => "failed",
    }
}

fn command_name(command: AgentCommand) -> &'static str {
    match command {
        AgentCommand::Start => "start",
        AgentCommand::Stop => "stop",
        AgentCommand::Toggle => "toggle",
        AgentCommand::Cancel => "cancel",
        AgentCommand::Status => "status",
        AgentCommand::Reset => "reset",
        AgentCommand::Shutdown => "shutdown",
    }
}

fn sanitize_message(error: impl Display) -> String {
    error.to_string().replace(['\r', '\n'], " ")
}

fn install_shutdown_handler() -> Result<CancellationToken, String> {
    let shutdown = CancellationToken::new();
    let handler_shutdown = shutdown.clone();
    ctrlc::set_handler(move || handler_shutdown.cancel())
        .map_err(|error| format!("failed to install Ctrl-C handler: {error}"))?;
    Ok(shutdown)
}

fn required_flag<'a>(args: &'a [String], flag: &str) -> Result<&'a str, String> {
    flag_value(args, flag).ok_or_else(|| format!("missing required flag: {flag}"))
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn print_help() {
    println!("Usage:");
    println!("  voin-cli agent daemon --model /path/to/model.bin [--endpoint 127.0.0.1:38741]");
    println!("  voin-cli agent start|stop|toggle|cancel|status|reset|shutdown [--endpoint 127.0.0.1:38741]");
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{AgentCommand, AgentReply, AgentRuntime};
    use voice_input_core::{
        AudioBuffer, AudioDevice, CancellationToken, DeterministicPostProcessor, OutputContext,
        ProcessedText, ProcessingContext, Recorder, RecorderError, RecordingOptions, SendReceipt,
        SessionCoordinator, SessionState, SinkError, TextSink, Transcriber, Transcript,
        TranscriptionError, TranscriptionOptions,
    };

    struct DummyRecorder {
        recording: bool,
    }

    impl Recorder for DummyRecorder {
        fn list_devices(&self) -> Result<Vec<AudioDevice>, RecorderError> {
            Ok(Vec::new())
        }

        fn start(&mut self, _options: &RecordingOptions) -> Result<(), RecorderError> {
            self.recording = true;
            Ok(())
        }

        fn stop(&mut self) -> Result<AudioBuffer, RecorderError> {
            if !self.recording {
                return Err(RecorderError::new("recorder is not running"));
            }
            self.recording = false;
            Ok(AudioBuffer::new(vec![0.0; 16_000], 16_000, 1))
        }

        fn cancel(&mut self) -> Result<(), RecorderError> {
            self.recording = false;
            Ok(())
        }
    }

    struct DummyTranscriber;

    impl Transcriber for DummyTranscriber {
        fn transcribe(
            &self,
            audio: &AudioBuffer,
            _options: &TranscriptionOptions,
            _cancel: &CancellationToken,
        ) -> Result<Transcript, TranscriptionError> {
            Ok(Transcript {
                text: "hello".to_owned(),
                language: Some("en".to_owned()),
                duration: audio.duration,
                segments: Vec::new(),
            })
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSink {
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl TextSink for RecordingSink {
        fn send(
            &self,
            text: &ProcessedText,
            _context: &OutputContext,
        ) -> Result<SendReceipt, SinkError> {
            self.sent.lock().unwrap().push(text.text.clone());
            Ok(SendReceipt {
                sink_name: "test".to_owned(),
                bytes_sent: text.text.len(),
                pasted: false,
            })
        }
    }

    fn runtime() -> AgentRuntime {
        let coordinator = SessionCoordinator::new(
            Box::new(DummyRecorder { recording: false }),
            Box::new(DummyTranscriber),
            Box::new(DeterministicPostProcessor),
            Box::new(RecordingSink::default()),
        );
        AgentRuntime::new(
            coordinator,
            RecordingOptions::default(),
            TranscriptionOptions::default(),
            ProcessingContext::default(),
            CancellationToken::new(),
        )
    }

    #[test]
    fn parses_supported_commands() {
        assert_eq!(AgentCommand::parse("toggle\n"), Ok(AgentCommand::Toggle));
        assert_eq!(AgentCommand::parse(" status "), Ok(AgentCommand::Status));
        assert!(AgentCommand::parse("unknown").is_err());
    }

    #[test]
    fn toggle_starts_and_stops_one_session() {
        let mut runtime = runtime();

        assert_eq!(
            runtime.handle(AgentCommand::Status),
            Ok(AgentReply::State(SessionState::Idle))
        );
        assert_eq!(
            runtime.handle(AgentCommand::Toggle),
            Ok(AgentReply::State(SessionState::Recording))
        );
        assert!(matches!(
            runtime.handle(AgentCommand::Toggle),
            Ok(AgentReply::Completed(_))
        ));
        assert_eq!(runtime.state(), SessionState::Idle);
    }

    #[test]
    fn cancel_returns_the_agent_to_idle() {
        let mut runtime = runtime();
        runtime
            .handle(AgentCommand::Start)
            .expect("recording must start");

        assert_eq!(
            runtime.handle(AgentCommand::Cancel),
            Ok(AgentReply::State(SessionState::Idle))
        );
    }

    #[test]
    fn toggle_starts_a_new_session_after_completion() {
        let mut runtime = runtime();
        runtime
            .handle(AgentCommand::Start)
            .expect("recording must start");
        runtime
            .handle(AgentCommand::Stop)
            .expect("recording must stop");

        assert_eq!(
            runtime.handle(AgentCommand::Toggle),
            Ok(AgentReply::State(SessionState::Recording))
        );
    }

    #[test]
    fn response_encoding_is_stable() {
        assert_eq!(
            super::render_reply(AgentReply::State(SessionState::Recording)),
            "OK state=recording"
        );
        assert_eq!(super::render_reply(AgentReply::Shutdown), "OK shutdown");
    }
}
