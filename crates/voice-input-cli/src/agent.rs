use std::fmt::Display;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
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

const MAX_COMMAND_BYTES: usize = 128;
const COMMAND_READ_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

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
            AgentCommand::Start => self.start_recording(),
            AgentCommand::Stop => self.stop_and_send(),
            AgentCommand::Toggle => match self.coordinator.state() {
                SessionState::Idle => self.start_recording(),
                SessionState::Recording => self.stop_and_send(),
                SessionState::Failed => {
                    self.coordinator.reset()?;
                    self.start_recording()
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

    fn start_recording(&mut self) -> Result<AgentReply, AppError> {
        self.operation_cancel.reset();
        self.coordinator.start_recording(&self.recording_options)?;
        Ok(AgentReply::State(self.coordinator.state()))
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

struct AgentRequest {
    command: AgentCommand,
    response: mpsc::Sender<String>,
}

struct SharedAgentState {
    state: Mutex<SessionState>,
    operation_cancel: CancellationToken,
    shutdown: CancellationToken,
}

impl SharedAgentState {
    fn new(runtime: &AgentRuntime, shutdown: &CancellationToken) -> Self {
        Self {
            state: Mutex::new(runtime.state()),
            operation_cancel: runtime.operation_cancel.clone(),
            shutdown: shutdown.clone(),
        }
    }

    fn set_state(&self, state: SessionState) -> io::Result<()> {
        *self
            .state
            .lock()
            .map_err(|_| io::Error::other("agent state mutex is poisoned"))? = state;
        Ok(())
    }
}

enum CommandDispatch {
    Queue(AgentCommand),
    Respond(String),
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

fn reserve_command(
    command: AgentCommand,
    shared: &SharedAgentState,
) -> Result<CommandDispatch, String> {
    let mut state = shared
        .state
        .lock()
        .map_err(|_| "agent state mutex is poisoned".to_owned())?;

    match command {
        AgentCommand::Status => Ok(CommandDispatch::Respond(format!(
            "OK state={}",
            state_name(*state)
        ))),
        AgentCommand::Shutdown => {
            shared.operation_cancel.cancel();
            shared.shutdown.cancel();
            Ok(CommandDispatch::Respond("OK shutdown".to_owned()))
        }
        AgentCommand::Start => {
            require_state(*state, SessionState::Idle)?;
            *state = SessionState::Recording;
            Ok(CommandDispatch::Queue(AgentCommand::Start))
        }
        AgentCommand::Stop => {
            require_state(*state, SessionState::Recording)?;
            *state = SessionState::Transcribing;
            Ok(CommandDispatch::Queue(AgentCommand::Stop))
        }
        AgentCommand::Toggle => match *state {
            SessionState::Idle => {
                *state = SessionState::Recording;
                Ok(CommandDispatch::Queue(AgentCommand::Start))
            }
            SessionState::Recording => {
                *state = SessionState::Transcribing;
                Ok(CommandDispatch::Queue(AgentCommand::Stop))
            }
            SessionState::Failed => {
                *state = SessionState::Recording;
                Ok(CommandDispatch::Queue(AgentCommand::Toggle))
            }
            actual => Err(busy_state_error(actual)),
        },
        AgentCommand::Cancel => match *state {
            SessionState::Recording => {
                *state = SessionState::Idle;
                Ok(CommandDispatch::Queue(AgentCommand::Cancel))
            }
            SessionState::Transcribing | SessionState::PostProcessing | SessionState::Sending => {
                shared.operation_cancel.cancel();
                Ok(CommandDispatch::Respond("OK cancel requested".to_owned()))
            }
            actual => Err(invalid_state_error(SessionState::Recording, actual)),
        },
        AgentCommand::Reset => {
            require_state(*state, SessionState::Failed)?;
            *state = SessionState::Idle;
            Ok(CommandDispatch::Queue(AgentCommand::Reset))
        }
    }
}

fn require_state(actual: SessionState, expected: SessionState) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(invalid_state_error(expected, actual))
    }
}

fn invalid_state_error(expected: SessionState, actual: SessionState) -> String {
    format!("invalid session state: expected {expected:?}, got {actual:?}")
}

fn busy_state_error(actual: SessionState) -> String {
    format!("agent is busy: state={}", state_name(actual))
}

fn daemon_command(args: &[String]) -> Result<(), String> {
    let model = required_flag(args, "--model")?;
    let endpoint = flag_value(args, "--endpoint").unwrap_or(DEFAULT_ENDPOINT);
    let shutdown = install_shutdown_handler()?;
    let operation_cancel = CancellationToken::new();
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
    let runtime = AgentRuntime::new(
        coordinator,
        RecordingOptions::default(),
        TranscriptionOptions::default(),
        ProcessingContext::default(),
        operation_cancel,
    );

    run_server(endpoint, runtime, &shutdown)
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
    runtime: AgentRuntime,
    shutdown: &CancellationToken,
) -> io::Result<()> {
    let listener = TcpListener::bind(endpoint)?;
    run_server_with_listener(listener, runtime, shutdown)
}

fn run_server_with_listener(
    listener: TcpListener,
    runtime: AgentRuntime,
    shutdown: &CancellationToken,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    eprintln!("voin agent listening on {}", listener.local_addr()?);

    let shared = Arc::new(SharedAgentState::new(&runtime, shutdown));
    let (requests, receiver) = mpsc::channel();
    let worker_shared = Arc::clone(&shared);
    let worker = thread::spawn(move || worker_loop(runtime, receiver, worker_shared));
    let mut connections = Vec::new();

    while !shutdown.is_cancelled() {
        reap_finished_connections(&mut connections);
        match listener.accept() {
            Ok((stream, _address)) => {
                let requests = requests.clone();
                let shared = Arc::clone(&shared);
                connections.push(thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, &requests, &shared) {
                        eprintln!("agent connection failed: {error}");
                    }
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                shared.operation_cancel.cancel();
                drop(requests);
                for connection in connections {
                    let _ = connection.join();
                }
                let _ = worker.join();
                return Err(error);
            }
        }
    }

    shared.operation_cancel.cancel();
    drop(requests);
    for connection in connections {
        let _ = connection.join();
    }
    let _ = worker.join();
    Ok(())
}

fn worker_loop(
    mut runtime: AgentRuntime,
    receiver: mpsc::Receiver<AgentRequest>,
    shared: Arc<SharedAgentState>,
) {
    while let Ok(request) = receiver.recv() {
        if shared.shutdown.is_cancelled() {
            let _ = request
                .response
                .send("ERR agent is shutting down".to_owned());
            continue;
        }

        let response = match runtime.handle(request.command) {
            Ok(reply) => render_reply(reply),
            Err(error) => format!("ERR {}", sanitize_message(error)),
        };
        if let Err(error) = shared.set_state(runtime.state()) {
            eprintln!("failed to update agent state: {error}");
        }
        let _ = request.response.send(response);
    }

    if runtime.state() == SessionState::Recording {
        let _ = runtime.handle(AgentCommand::Cancel);
    }
    if let Err(error) = shared.set_state(runtime.state()) {
        eprintln!("failed to update agent state: {error}");
    }
}

fn reap_finished_connections(connections: &mut Vec<thread::JoinHandle<()>>) {
    let mut active = Vec::with_capacity(connections.len());
    for connection in connections.drain(..) {
        if connection.is_finished() {
            let _ = connection.join();
        } else {
            active.push(connection);
        }
    }
    *connections = active;
}

fn handle_connection(
    mut stream: TcpStream,
    requests: &mpsc::Sender<AgentRequest>,
    shared: &SharedAgentState,
) -> io::Result<()> {
    stream.set_read_timeout(Some(COMMAND_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(COMMAND_WRITE_TIMEOUT))?;

    let Some(line) = read_command_line(&mut stream)? else {
        return Ok(());
    };
    let command = match AgentCommand::parse(&line) {
        Ok(command) => command,
        Err(error) => {
            write_response(&mut stream, &format!("ERR {error}"))?;
            return Ok(());
        }
    };

    match reserve_command(command, shared) {
        Ok(CommandDispatch::Respond(response)) => write_response(&mut stream, &response),
        Ok(CommandDispatch::Queue(command)) => {
            let (response, receiver) = mpsc::channel();
            requests
                .send(AgentRequest { command, response })
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "agent worker stopped"))?;
            let response = receiver
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "agent worker stopped"))?;
            write_response(&mut stream, &response)
        }
        Err(error) => write_response(&mut stream, &format!("ERR {error}")),
    }
}

fn read_command_line(stream: &mut TcpStream) -> io::Result<Option<String>> {
    read_command_line_from(stream)
}

fn read_command_line_from(reader: &mut impl Read) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(MAX_COMMAND_BYTES);
    loop {
        let mut byte = [0_u8; 1];
        match reader.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => break,
            1 if byte[0] == b'\n' => break,
            1 => {
                if bytes.len() >= MAX_COMMAND_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "command is too long",
                    ));
                }
                bytes.push(byte[0]);
            }
            _ => unreachable!("a one-byte buffer cannot receive multiple bytes"),
        }
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "command is not valid UTF-8"))
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

    let response = read_command_line(&mut stream)
        .map_err(|error| format!("failed to read agent response: {error}"))?
        .ok_or_else(|| "agent closed the connection without a response".to_owned())?;
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
    use std::io::{Cursor, ErrorKind, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

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

    struct BlockingTranscriber {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl Transcriber for BlockingTranscriber {
        fn transcribe(
            &self,
            audio: &AudioBuffer,
            _options: &TranscriptionOptions,
            _cancel: &CancellationToken,
        ) -> Result<Transcript, TranscriptionError> {
            self.entered.wait();
            self.release.wait();
            Ok(Transcript {
                text: "hello".to_owned(),
                language: Some("en".to_owned()),
                duration: audio.duration,
                segments: Vec::new(),
            })
        }
    }

    struct TestServer {
        endpoint: String,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn new(runtime: AgentRuntime) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("test listener must bind");
            let endpoint = listener
                .local_addr()
                .expect("test listener must have an address")
                .to_string();
            let shutdown = CancellationToken::new();
            let thread = thread::spawn(move || {
                super::run_server_with_listener(listener, runtime, &shutdown)
                    .expect("test server must run successfully");
            });

            Self {
                endpoint,
                thread: Some(thread),
            }
        }

        fn send(&self, command: AgentCommand) -> Result<String, String> {
            super::send_command(&self.endpoint, command)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.send(AgentCommand::Shutdown);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("test server thread must exit");
            }
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

    fn runtime_with_transcriber(transcriber: Box<dyn Transcriber>) -> AgentRuntime {
        let coordinator = SessionCoordinator::new(
            Box::new(DummyRecorder { recording: false }),
            transcriber,
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

    fn runtime() -> AgentRuntime {
        runtime_with_transcriber(Box::new(DummyTranscriber))
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
    fn command_reader_enforces_limit_without_newline() {
        let mut input = Cursor::new(vec![b'x'; super::MAX_COMMAND_BYTES + 1]);
        let error = super::read_command_line_from(&mut input)
            .expect_err("an oversized command must be rejected");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "command is too long");
    }

    #[test]
    fn command_reader_rejects_invalid_utf8() {
        let mut input = Cursor::new(vec![0xff, b'\n']);
        let error =
            super::read_command_line_from(&mut input).expect_err("invalid UTF-8 must be rejected");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "command is not valid UTF-8");
    }

    #[test]
    fn server_accepts_status_and_cancel_while_transcribing() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let server = TestServer::new(runtime_with_transcriber(Box::new(BlockingTranscriber {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })));

        assert_eq!(
            server.send(AgentCommand::Start),
            Ok("OK state=recording".to_owned())
        );
        let endpoint = server.endpoint.clone();
        let stop = thread::spawn(move || super::send_command(&endpoint, AgentCommand::Stop));

        entered.wait();
        assert_eq!(
            server.send(AgentCommand::Status),
            Ok("OK state=transcribing".to_owned())
        );
        assert_eq!(
            server.send(AgentCommand::Toggle),
            Err("agent is busy: state=transcribing".to_owned())
        );
        assert_eq!(
            server.send(AgentCommand::Cancel),
            Ok("OK cancel requested".to_owned())
        );

        release.wait();
        assert_eq!(
            stop.join().expect("stop client must finish"),
            Err("operation cancelled".to_owned())
        );
        assert_eq!(
            server.send(AgentCommand::Status),
            Ok("OK state=idle".to_owned())
        );
    }

    #[test]
    fn malformed_connections_do_not_stop_the_server() {
        let server = TestServer::new(runtime());
        let _slow_connection =
            TcpStream::connect(&server.endpoint).expect("slow connection must connect");

        assert_eq!(
            server.send(AgentCommand::Status),
            Ok("OK state=idle".to_owned())
        );

        let mut invalid_utf8 =
            TcpStream::connect(&server.endpoint).expect("invalid connection must connect");
        invalid_utf8
            .write_all(&[0xff, b'\n'])
            .expect("invalid command must be written");

        let mut oversized =
            TcpStream::connect(&server.endpoint).expect("oversized connection must connect");
        oversized
            .write_all(&[b'x'; super::MAX_COMMAND_BYTES + 1])
            .expect("oversized command must be written");

        assert_eq!(
            server.send(AgentCommand::Status),
            Ok("OK state=idle".to_owned())
        );
    }

    #[test]
    fn starting_a_session_resets_operation_cancellation() {
        let mut runtime = runtime();
        runtime.operation_cancel.cancel();

        assert_eq!(
            runtime.handle(AgentCommand::Start),
            Ok(AgentReply::State(SessionState::Recording))
        );
        assert!(matches!(
            runtime.handle(AgentCommand::Toggle),
            Ok(AgentReply::Completed(_))
        ));
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
