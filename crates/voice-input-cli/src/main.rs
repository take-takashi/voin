use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

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

    let mut recorder = CpalRecorder::new();
    let options = RecordingOptions {
        max_duration: Duration::from_secs(duration_seconds),
        ..RecordingOptions::default()
    };

    recorder
        .start(&options)
        .map_err(|error| format!("failed to start recording: {error}"))?;
    eprintln!("recording for {duration_seconds} seconds...");
    thread::sleep(Duration::from_secs(duration_seconds));

    let audio = recorder
        .stop()
        .map_err(|error| format!("failed to stop recording: {error}"))?;
    write_wav(output, &audio)?;
    eprintln!("saved recording to {output}");
    Ok(())
}

fn transcribe_command(args: &[String]) -> Result<(), String> {
    let input = required_flag(args, "--input")?;
    let model = required_flag(args, "--model")?;
    let sink_name = flag_value(args, "--sink").unwrap_or("stdout");
    let format = parse_output_format(flag_value(args, "--format").unwrap_or("plain"))?;
    let audio = read_wav(input)?;

    let transcriber = WhisperTranscriber::from_model_path(model)
        .map_err(|error| format!("failed to initialize transcriber: {error}"))?;
    let transcript = transcriber
        .transcribe(
            &audio,
            &TranscriptionOptions {
                language: LanguageMode::Auto,
                ..TranscriptionOptions::default()
            },
            &CancellationToken::new(),
        )
        .map_err(|error| format!("transcription failed: {error}"))?;
    let processed = DeterministicPostProcessor
        .process(transcript, &ProcessingContext::default())
        .map_err(|error| format!("post-processing failed: {error}"))?;

    if processed.text.is_empty() {
        return Err("transcription result is empty".to_owned());
    }

    let output_context = OutputContext::new("cli-transcribe");
    match sink_name {
        "stdout" => {
            let sink = StdoutSink::new(format);
            sink.send(&processed, &output_context)
                .map_err(|error| format!("stdout output failed: {error}"))?;
        }
        "clipboard" => {
            if format == OutputFormat::Json {
                return Err("--format json requires --sink stdout".to_owned());
            }
            let sink = ClipboardSink::new()
                .map_err(|error| format!("failed to initialize clipboard: {error}"))?;
            sink.send(&processed, &output_context)
                .map_err(|error| format!("clipboard output failed: {error}"))?;
        }
        _ => return Err("--sink must be stdout or clipboard".to_owned()),
    }
    Ok(())
}

fn write_wav(path: &str, audio: &AudioBuffer) -> Result<(), String> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: audio.sample_rate_hz,
        bits_per_sample: 16,
        sample_format: WavSampleFormat::Int,
    };
    let mut writer =
        WavWriter::create(path, spec).map_err(|error| format!("failed to create WAV: {error}"))?;

    for sample in &audio.samples {
        let sample = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        writer
            .write_sample(sample)
            .map_err(|error| format!("failed to write WAV: {error}"))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("failed to finalize WAV: {error}"))?;
    Ok(())
}

fn read_wav(path: &str) -> Result<AudioBuffer, String> {
    let mut reader =
        WavReader::open(path).map_err(|error| format!("failed to open WAV: {error}"))?;
    let spec = reader.spec();

    if spec.channels != 1 || spec.sample_rate != 16_000 {
        return Err("WAV input must be 16kHz mono".to_owned());
    }

    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (WavSampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| {
                sample
                    .map(|sample| f32::from(sample) / 32_768.0)
                    .map_err(|error| format!("failed to read WAV sample: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        (WavSampleFormat::Float, 32) => reader
            .samples::<f32>()
            .map(|sample| sample.map_err(|error| format!("failed to read WAV sample: {error}")))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("WAV input must use 16-bit integer or 32-bit float samples".to_owned()),
    };

    Ok(AudioBuffer::new(samples, spec.sample_rate, spec.channels))
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
    println!("voin - local Japanese voice input");
    println!();
    println!("Usage:");
    println!("  voin doctor");
    println!("  voin devices list");
    println!("  voin record --output /tmp/voice.wav [--duration 5]");
    println!(
        "  voin transcribe --input /tmp/voice.wav --model /path/to/model.bin [--sink stdout|clipboard] [--format plain|json]"
    );
}
