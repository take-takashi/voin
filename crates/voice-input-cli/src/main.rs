use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use hound::{SampleFormat as WavSampleFormat, WavReader, WavSpec, WavWriter};
use voice_input_core::{
    AudioBuffer, CancellationToken, DeterministicPostProcessor, LanguageMode, OutputContext,
    OutputFormat, PostProcessor, ProcessingContext, Recorder, RecordingOptions, TextSink,
    Transcriber, TranscriptionOptions,
};
use voice_input_recorder_cpal::CpalRecorder;
use voice_input_sinks::{ClipboardSink, StdoutSink};
use voice_input_transcriber_whisper::WhisperTranscriber;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };

    match command {
        "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "devices" if args.len() == 2 && args.get(1).map(String::as_str) == Some("list") => {
            list_devices()
        }
        "doctor" if args.len() == 1 => doctor(),
        "record" => record_command(&args),
        "transcribe" => transcribe_command(&args),
        _ => {
            print_help();
            Err("unknown command".to_owned())
        }
    }
}

fn list_devices() -> Result<(), String> {
    let recorder = CpalRecorder::new();
    let devices = recorder
        .list_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;

    if devices.is_empty() {
        println!("no input devices");
        return Ok(());
    }

    for device in devices {
        let marker = if device.is_default { "*" } else { " " };
        println!("{marker} {}", device.name);
    }
    Ok(())
}

fn doctor() -> Result<(), String> {
    let recorder = CpalRecorder::new();
    let devices = recorder
        .list_devices()
        .map_err(|error| format!("input device check failed: {error}"))?;

    println!("input_devices: {}", devices.len());
    println!("recorder: cpal");
    println!("target_audio: 16000Hz mono f32");
    println!("stdout_sink: available");
    println!("whisper_model: configure a model path before transcription");

    if devices.is_empty() {
        return Err("no input device is available".to_owned());
    }

    Ok(())
}

fn record_command(args: &[String]) -> Result<(), String> {
    let output = required_flag(args, "--output")?;
    let duration_seconds = flag_value(args, "--duration")
        .unwrap_or("5")
        .parse::<u64>()
        .map_err(|_| "--duration must be a positive integer")?;

    if duration_seconds == 0 {
        return Err("--duration must be a positive integer".to_owned());
    }

    let cancel = install_cancellation_handler()?;
    let mut recorder = CpalRecorder::new();
    let options = RecordingOptions {
        max_duration: Duration::from_secs(duration_seconds),
        ..RecordingOptions::default()
    };

    recorder
        .start(&options)
        .map_err(|error| format!("failed to start recording: {error}"))?;
    if cancel.is_cancelled() {
        recorder
            .cancel()
            .map_err(|error| format!("failed to cancel recording: {error}"))?;
        return Err("operation cancelled".to_owned());
    }

    eprintln!("recording for {duration_seconds} seconds...");
    let duration = Duration::from_secs(duration_seconds);
    let started = Instant::now();
    while started.elapsed() < duration {
        if cancel.is_cancelled() {
            recorder
                .cancel()
                .map_err(|error| format!("failed to cancel recording: {error}"))?;
            return Err("operation cancelled".to_owned());
        }
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }

    if cancel.is_cancelled() {
        recorder
            .cancel()
            .map_err(|error| format!("failed to cancel recording: {error}"))?;
        return Err("operation cancelled".to_owned());
    }

    let audio = recorder
        .stop()
        .map_err(|error| format!("failed to stop recording: {error}"))?;
    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }

    write_wav(output, &audio, &cancel)?;
    eprintln!("saved recording to {output}");
    Ok(())
}

fn transcribe_command(args: &[String]) -> Result<(), String> {
    let input = required_flag(args, "--input")?;
    let model = required_flag(args, "--model")?;
    let sink_name = flag_value(args, "--sink").unwrap_or("stdout");
    let format = parse_output_format(flag_value(args, "--format").unwrap_or("plain"))?;
    let cancel = install_cancellation_handler()?;
    let audio = read_wav(input, &cancel)?;

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }

    let transcriber = WhisperTranscriber::from_model_path(model)
        .map_err(|error| format!("failed to initialize transcriber: {error}"))?;
    let transcript = match transcriber.transcribe(
        &audio,
        &TranscriptionOptions {
            language: LanguageMode::Auto,
            ..TranscriptionOptions::default()
        },
        &cancel,
    ) {
        Ok(transcript) => transcript,
        Err(_error) if cancel.is_cancelled() => return Err("operation cancelled".to_owned()),
        Err(error) => return Err(format!("transcription failed: {error}")),
    };

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }

    let processed = DeterministicPostProcessor
        .process(transcript, &ProcessingContext::default())
        .map_err(|error| format!("post-processing failed: {error}"))?;

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    if processed.text.is_empty() {
        return Err("transcription result is empty".to_owned());
    }

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    let output_context = OutputContext::new("cli-transcribe");
    let send_result = match sink_name {
        "stdout" => {
            let sink = StdoutSink::new(format);
            sink.send(&processed, &output_context)
                .map_err(|error| format!("stdout output failed: {error}"))
        }
        "clipboard" => {
            if format == OutputFormat::Json {
                return Err("--format json requires --sink stdout".to_owned());
            }
            let sink = ClipboardSink::new()
                .map_err(|error| format!("failed to initialize clipboard: {error}"))?;
            sink.send(&processed, &output_context)
                .map_err(|error| format!("clipboard output failed: {error}"))
        }
        _ => return Err("--sink must be stdout or clipboard".to_owned()),
    };

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    send_result?;
    Ok(())
}

fn write_wav(path: &str, audio: &AudioBuffer, cancel: &CancellationToken) -> Result<(), String> {
    let output_path = Path::new(path);
    let mut temporary = TemporaryOutput::new(temporary_output_path(output_path));
    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    let spec = WavSpec {
        channels: 1,
        sample_rate: audio.sample_rate_hz,
        bits_per_sample: 16,
        sample_format: WavSampleFormat::Int,
    };
    let mut writer = WavWriter::create(temporary.path(), spec)
        .map_err(|error| format!("failed to create temporary WAV: {error}"))?;

    for sample in &audio.samples {
        if cancel.is_cancelled() {
            return Err("operation cancelled".to_owned());
        }
        let sample = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        writer
            .write_sample(sample)
            .map_err(|error| format!("failed to write WAV: {error}"))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("failed to finalize WAV: {error}"))?;

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    fs::rename(temporary.path(), output_path)
        .map_err(|error| format!("failed to move WAV into place: {error}"))?;
    temporary.commit();
    Ok(())
}

struct TemporaryOutput {
    path: Option<PathBuf>,
}

impl TemporaryOutput {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("temporary output path must be available")
    }

    fn commit(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn temporary_output_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("voin-output");
    output.with_file_name(format!(".{name}.{}.part", std::process::id()))
}

fn read_wav(path: &str, cancel: &CancellationToken) -> Result<AudioBuffer, String> {
    let mut reader =
        WavReader::open(path).map_err(|error| format!("failed to open WAV: {error}"))?;
    let spec = reader.spec();

    if spec.channels != 1 || spec.sample_rate != 16_000 {
        return Err("WAV input must be 16kHz mono".to_owned());
    }

    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (WavSampleFormat::Int, 16) => {
            let mut samples = Vec::new();
            for sample in reader.samples::<i16>() {
                if cancel.is_cancelled() {
                    return Err("operation cancelled".to_owned());
                }
                let sample =
                    sample.map_err(|error| format!("failed to read WAV sample: {error}"))?;
                samples.push(f32::from(sample) / 32_768.0);
            }
            samples
        }
        (WavSampleFormat::Float, 32) => {
            let mut samples = Vec::new();
            for sample in reader.samples::<f32>() {
                if cancel.is_cancelled() {
                    return Err("operation cancelled".to_owned());
                }
                samples
                    .push(sample.map_err(|error| format!("failed to read WAV sample: {error}"))?);
            }
            samples
        }
        _ => return Err("WAV input must use 16-bit integer or 32-bit float samples".to_owned()),
    };

    if cancel.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    Ok(AudioBuffer::new(samples, spec.sample_rate, spec.channels))
}

fn install_cancellation_handler() -> Result<CancellationToken, String> {
    let cancel = CancellationToken::new();
    let handler_cancel = cancel.clone();
    ctrlc::set_handler(move || handler_cancel.cancel())
        .map_err(|error| format!("failed to install Ctrl-C handler: {error}"))?;
    Ok(cancel)
}

fn parse_output_format(value: &str) -> Result<OutputFormat, String> {
    match value {
        "plain" => Ok(OutputFormat::Plain),
        "json" => Ok(OutputFormat::Json),
        _ => Err("--format must be plain or json".to_owned()),
    }
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
    println!("voin-cli - local Japanese voice input");
    println!();
    println!("Usage:");
    println!("  voin-cli doctor");
    println!("  voin-cli devices list");
    println!("  voin-cli record --output /tmp/voice.wav [--duration 5]");
    println!(
        "  voin-cli transcribe --input /tmp/voice.wav --model /path/to/model.bin [--sink stdout|clipboard] [--format plain|json]"
    );
}

#[cfg(test)]
mod tests {
    use super::{temporary_output_path, write_wav};
    use std::fs;
    use voice_input_core::{AudioBuffer, CancellationToken};

    #[test]
    fn cancelled_wav_write_leaves_no_output_or_partial_file() {
        let output = std::env::temp_dir().join(format!(
            "voin-cancelled-{}-{}.wav",
            std::process::id(),
            "test"
        ));
        let temporary = temporary_output_path(&output);
        let _ = fs::remove_file(&output);
        let _ = fs::remove_file(&temporary);

        let audio = AudioBuffer::new(vec![0.0; 16_000], 16_000, 1);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = write_wav(output.to_str().unwrap(), &audio, &cancel)
            .expect_err("cancelled output must fail");

        assert_eq!(error, "operation cancelled");
        assert!(!output.exists());
        assert!(!temporary.exists());
    }
}
