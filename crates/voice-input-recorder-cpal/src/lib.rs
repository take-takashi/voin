use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use voice_input_core::{AudioBuffer, AudioDevice, Recorder, RecorderError, RecordingOptions};

#[derive(Default)]
struct CaptureBuffer {
    samples: Vec<f32>,
    max_samples: usize,
}

/// CPALの入力を共通Recorder traitへ変換するアダプターです。
pub struct CpalRecorder {
    host: cpal::Host,
    stream: Option<Stream>,
    capture: Arc<Mutex<CaptureBuffer>>,
    callback_error: Arc<Mutex<Option<String>>>,
    started_at: Option<SystemTime>,
    native_sample_rate_hz: u32,
    target_sample_rate_hz: u32,
}

impl Default for CpalRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl CpalRecorder {
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
            stream: None,
            capture: Arc::new(Mutex::new(CaptureBuffer::default())),
            callback_error: Arc::new(Mutex::new(None)),
            started_at: None,
            native_sample_rate_hz: 0,
            target_sample_rate_hz: 16_000,
        }
    }

    fn select_device(&self, requested_name: Option<&str>) -> Result<cpal::Device, RecorderError> {
        match requested_name {
            None => self
                .host
                .default_input_device()
                .ok_or_else(|| RecorderError::new("default input device is unavailable")),
            Some(requested_name) => {
                let mut names = Vec::new();
                let devices = self.host.input_devices().map_err(|error| {
                    RecorderError::new(format!("failed to list input devices: {error}"))
                })?;

                for device in devices {
                    let name = device.to_string();
                    if name == requested_name {
                        return Ok(device);
                    }
                    names.push(name);
                }

                Err(RecorderError::new(format!(
                    "input device '{requested_name}' is unavailable; candidates: {}",
                    names.join(", ")
                )))
            }
        }
    }

    fn build_stream(
        &self,
        device: &cpal::Device,
        config: StreamConfig,
        sample_format: SampleFormat,
        channels: u16,
    ) -> Result<Stream, RecorderError> {
        let stream = match sample_format {
            SampleFormat::F32 => {
                let capture = Arc::clone(&self.capture);
                let errors = Arc::clone(&self.callback_error);
                device.build_input_stream(
                    config,
                    move |data: &[f32], _| {
                        append_interleaved(data, channels, &capture, |sample| *sample)
                    },
                    move |error| store_callback_error(&errors, error.to_string()),
                    None,
                )
            }
            SampleFormat::F64 => {
                let capture = Arc::clone(&self.capture);
                let errors = Arc::clone(&self.callback_error);
                device.build_input_stream(
                    config,
                    move |data: &[f64], _| {
                        append_interleaved(data, channels, &capture, |sample| *sample as f32)
                    },
                    move |error| store_callback_error(&errors, error.to_string()),
                    None,
                )
            }
            SampleFormat::I16 => {
                let capture = Arc::clone(&self.capture);
                let errors = Arc::clone(&self.callback_error);
                device.build_input_stream(
                    config,
                    move |data: &[i16], _| {
                        append_interleaved(data, channels, &capture, |sample| {
                            f32::from(*sample) / 32_768.0
                        })
                    },
                    move |error| store_callback_error(&errors, error.to_string()),
                    None,
                )
            }
            SampleFormat::U16 => {
                let capture = Arc::clone(&self.capture);
                let errors = Arc::clone(&self.callback_error);
                device.build_input_stream(
                    config,
                    move |data: &[u16], _| {
                        append_interleaved(data, channels, &capture, |sample| {
                            (f32::from(*sample) - 32_768.0) / 32_768.0
                        })
                    },
                    move |error| store_callback_error(&errors, error.to_string()),
                    None,
                )
            }
            unsupported => {
                return Err(RecorderError::new(format!(
                    "unsupported input sample format: {unsupported:?}"
                )))
            }
        }
        .map_err(|error| RecorderError::new(format!("failed to build input stream: {error}")))?;

        Ok(stream)
    }

    fn clear_capture(&self) {
        if let Ok(mut capture) = self.capture.lock() {
            capture.samples.clear();
            capture.max_samples = 0;
        }
    }

    fn take_capture(&self) -> Result<Vec<f32>, RecorderError> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| RecorderError::new("capture buffer mutex is poisoned"))?;
        Ok(std::mem::take(&mut capture.samples))
    }
}

impl Recorder for CpalRecorder {
    fn list_devices(&self) -> Result<Vec<AudioDevice>, RecorderError> {
        let default_name = self
            .host
            .default_input_device()
            .map(|device| device.to_string());
        let devices = self.host.input_devices().map_err(|error| {
            RecorderError::new(format!("failed to list input devices: {error}"))
        })?;

        let mut result = Vec::new();
        for device in devices {
            let name = device.to_string();
            result.push(AudioDevice {
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
            });
        }
        Ok(result)
    }

    fn start(&mut self, options: &RecordingOptions) -> Result<(), RecorderError> {
        if self.stream.is_some() {
            return Err(RecorderError::new("recorder is already running"));
        }
        if options.sample_rate_hz == 0 || options.channels == 0 {
            return Err(RecorderError::new(
                "target sample rate and channel count must be positive",
            ));
        }

        let device = self.select_device(options.device_name.as_deref())?;
        let supported_config = device
            .default_input_config()
            .map_err(|error| RecorderError::new(format!("failed to get input config: {error}")))?;
        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();
        let channels = config.channels;
        let native_sample_rate_hz = config.sample_rate;

        if channels == 0 || native_sample_rate_hz == 0 {
            return Err(RecorderError::new(
                "input device returned an invalid stream config",
            ));
        }

        let max_samples = (options.max_duration.as_secs_f64() * f64::from(native_sample_rate_hz))
            .floor() as usize;
        if max_samples == 0 {
            return Err(RecorderError::new(
                "maximum recording duration must be positive",
            ));
        }

        {
            let mut capture = self
                .capture
                .lock()
                .map_err(|_| RecorderError::new("capture buffer mutex is poisoned"))?;
            capture.samples.clear();
            capture.max_samples = max_samples;
        }
        *self
            .callback_error
            .lock()
            .map_err(|_| RecorderError::new("callback error mutex is poisoned"))? = None;

        let stream = self.build_stream(&device, config, sample_format, channels)?;
        stream.play().map_err(|error| {
            RecorderError::new(format!("failed to start input stream: {error}"))
        })?;

        self.stream = Some(stream);
        self.started_at = Some(SystemTime::now());
        self.native_sample_rate_hz = native_sample_rate_hz;
        self.target_sample_rate_hz = options.sample_rate_hz;
        Ok(())
    }

    fn stop(&mut self) -> Result<AudioBuffer, RecorderError> {
        let stream = self
            .stream
            .take()
            .ok_or_else(|| RecorderError::new("recorder is not running"))?;
        drop(stream);

        if let Some(error) = self
            .callback_error
            .lock()
            .map_err(|_| RecorderError::new("callback error mutex is poisoned"))?
            .take()
        {
            self.clear_capture();
            self.started_at = None;
            return Err(RecorderError::new(format!("input stream failed: {error}")));
        }

        let samples = self.take_capture()?;
        let started_at = self.started_at.take().unwrap_or_else(SystemTime::now);
        let resampled = resample_mono(
            &samples,
            self.native_sample_rate_hz,
            self.target_sample_rate_hz,
        );
        let mut audio = AudioBuffer::new(resampled, self.target_sample_rate_hz, 1);
        audio.started_at = started_at;
        Ok(audio)
    }

    fn cancel(&mut self) -> Result<(), RecorderError> {
        if self.stream.is_none() {
            return Err(RecorderError::new("recorder is not running"));
        }

        self.stream.take();
        self.started_at = None;
        self.clear_capture();
        Ok(())
    }
}

fn store_callback_error(slot: &Arc<Mutex<Option<String>>>, message: String) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(message);
    }
}

fn append_interleaved<T>(
    data: &[T],
    channels: u16,
    capture: &Arc<Mutex<CaptureBuffer>>,
    convert: impl Fn(&T) -> f32,
) {
    let channels = usize::from(channels);
    if channels == 0 {
        return;
    }

    if let Ok(mut capture) = capture.lock() {
        for frame in data.chunks_exact(channels) {
            if capture.samples.len() >= capture.max_samples {
                break;
            }

            let sum = frame.iter().map(&convert).sum::<f32>();
            capture.samples.push(sum / channels as f32);
        }
    }
}

fn resample_mono(input: &[f32], from_sample_rate_hz: u32, to_sample_rate_hz: u32) -> Vec<f32> {
    if input.is_empty() || from_sample_rate_hz == 0 || to_sample_rate_hz == 0 {
        return Vec::new();
    }
    if from_sample_rate_hz == to_sample_rate_hz {
        return input.to_vec();
    }

    let output_len = (input.len() as u64 * u64::from(to_sample_rate_hz)
        / u64::from(from_sample_rate_hz)) as usize;
    let mut output = Vec::with_capacity(output_len);

    for output_index in 0..output_len {
        let source_position =
            output_index as f64 * f64::from(from_sample_rate_hz) / f64::from(to_sample_rate_hz);
        let left_index = source_position.floor() as usize;
        let right_index = (left_index + 1).min(input.len() - 1);
        let fraction = (source_position - left_index as f64) as f32;
        output.push(input[left_index] * (1.0 - fraction) + input[right_index] * fraction);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{append_interleaved, resample_mono, CaptureBuffer};
    use std::sync::{Arc, Mutex};

    #[test]
    fn mixes_interleaved_samples_into_mono() {
        let capture = Arc::new(Mutex::new(CaptureBuffer {
            samples: Vec::new(),
            max_samples: 2,
        }));

        append_interleaved(&[1.0_f32, -1.0, 0.5, 0.5], 2, &capture, |sample| *sample);

        assert_eq!(capture.lock().unwrap().samples, [0.0, 0.5]);
    }

    #[test]
    fn resamples_linear_ramp() {
        let result = resample_mono(&[0.0, 1.0, 2.0, 3.0], 4, 2);

        assert_eq!(result, [0.0, 2.0]);
    }
}
