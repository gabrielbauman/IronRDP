use std::borrow::Cow;
use std::collections::VecDeque;

use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu, WaveFormat};
use tracing::{debug, error, info, trace, warn};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;
use web_sys::{AudioBuffer, AudioContext, Event, GainNode};

use crate::error::IronError;

/// Web Audio API backend for RDP audio playback
///
/// Features:
/// - PCM 16-bit audio format support
/// - Sample rate conversion for compatibility with any RDP server
/// - Channel count conversion (mono/stereo)
/// - User gesture activation handling for Web Audio policy
#[derive(Debug)]
pub(crate) struct WebAudioBackend {
    audio_context: AudioContext,
    gain_node: GainNode,
    context_sample_rate: f32,
    supported_formats: Vec<AudioFormat>,
    volume: f32,
    pitch: f32,
    is_active: bool,
    pending_audio_data: VecDeque<(Vec<u8>, AudioFormat)>,
    audio_queue: AudioQueue,
}

#[derive(Debug)]
struct AudioQueue {
    context: AudioContext,
    current_time: f64,
}

impl WebAudioBackend {
    /// Create a new WebAudioBackend with the specified sample rate
    pub(crate) fn new(sample_rate: Option<f32>) -> Result<Self, IronError> {
        let audio_context = AudioContext::new().map_err(|e| {
            anyhow::Error::msg(format!(
                "failed to create Web Audio API context (check browser support and user gesture requirement): {e:?}"
            ))
        })?;

        let gain_node = audio_context
            .create_gain()
            .map_err(|e| anyhow::Error::msg(format!("failed to create Web Audio gain node: {e:?}")))?;

        // Connect gain node to destination
        gain_node
            .connect_with_audio_node(&audio_context.destination())
            .map_err(|e| anyhow::Error::msg(format!("failed to connect gain node to audio destination: {e:?}")))?;

        let context_sample_rate = audio_context.sample_rate();
        let requested_rate = sample_rate.unwrap_or(context_sample_rate);

        let supported_sample_rates = Self::supported_sample_rates(&audio_context);
        let supported_formats = Self::create_supported_formats(requested_rate, &supported_sample_rates);

        let audio_queue = AudioQueue {
            context: audio_context.clone(),
            current_time: audio_context.current_time(),
        };

        let backend = Self {
            audio_context: audio_context.clone(),
            gain_node,
            context_sample_rate,
            supported_formats,
            volume: 1.0,
            pitch: 1.0,
            is_active: true,
            pending_audio_data: VecDeque::new(),
            audio_queue,
        };

        info!(
            "WebAudioBackend initialized: {} supported formats, context sample rate: {}Hz",
            backend.supported_formats.len(),
            backend.context_sample_rate
        );
        for (i, format) in backend.supported_formats.iter().enumerate() {
            info!(
                "Audio format {}: {:?} {}Hz {}ch ({}bps)",
                i, format.format, format.n_samples_per_sec, format.n_channels, format.bits_per_sample
            );
        }

        // Set up one-time user gesture listener on document
        Self::setup_user_gesture_listener(audio_context);

        Ok(backend)
    }

    /// Return the sample rates to advertise for format negotiation.
    #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn supported_sample_rates(audio_context: &AudioContext) -> Vec<u32> {
        let mut rates = vec![22050, 44100, 48000];
        let native_rate = audio_context.sample_rate().round() as u32;
        if !rates.contains(&native_rate) {
            rates.push(native_rate);
            rates.sort_unstable();
        }
        rates
    }

    /// Create the list of audio formats supported by the web backend.
    /// Only advertises PCM formats that we can actually decode.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn create_supported_formats(preferred_rate: f32, supported_rates: &[u32]) -> Vec<AudioFormat> {
        let mut formats = Vec::new();

        // Priority order: preferred rate first, then other supported rates
        let mut rates_to_add = vec![preferred_rate.round() as u32];
        for &rate in supported_rates {
            if rate != preferred_rate.round() as u32 {
                rates_to_add.push(rate);
            }
        }

        // Only add PCM formats - these are what we can actually decode
        // and what 95%+ of RDP servers actually support
        for &rate in &rates_to_add {
            // PCM 16-bit stereo (primary format)
            formats.push(AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: rate,
                n_avg_bytes_per_sec: rate * 2 * 2,
                n_block_align: 4,
                bits_per_sample: 16,
                data: Some(Vec::new()),
            });

            // PCM 16-bit mono (fallback)
            formats.push(AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 1,
                n_samples_per_sec: rate,
                n_avg_bytes_per_sec: rate * 2,
                n_block_align: 2,
                bits_per_sample: 16,
                data: Some(Vec::new()),
            });
        }

        info!(
            "Created {} PCM audio formats for sample rates: {:?}",
            formats.len(),
            rates_to_add
        );

        formats
    }

    /// Convert sample rate using linear interpolation
    ///
    /// Linear interpolation is sufficient for RDP audio quality requirements.
    /// Future enhancement: Consider higher-quality resampling for specialized use cases.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn convert_sample_rate(input_samples: &[f32], from_rate: u32, to_rate: u32, channels: u16) -> Vec<f32> {
        if from_rate == to_rate || channels == 0 || input_samples.is_empty() {
            return input_samples.to_vec();
        }

        let ratio = f64::from(from_rate) / f64::from(to_rate);
        let input_frames = input_samples.len() / channels as usize;
        let output_frames = (input_frames as f64 / ratio).round() as usize;
        let mut output = Vec::with_capacity(output_frames * channels as usize);

        for output_frame in 0..output_frames {
            let input_pos = output_frame as f64 * ratio;
            let input_frame = input_pos.floor() as usize;
            let frac = input_pos.fract() as f32;

            for channel in 0..channels as usize {
                let sample1_idx = input_frame * channels as usize + channel;
                let sample2_idx = ((input_frame + 1).min(input_frames - 1)) * channels as usize + channel;

                let sample1 = input_samples.get(sample1_idx).copied().unwrap_or(0.0);
                let sample2 = input_samples.get(sample2_idx).copied().unwrap_or(sample1);

                // Linear interpolation
                let interpolated = sample1 + (sample2 - sample1) * frac;
                output.push(interpolated);
            }
        }

        output
    }

    /// Convert mono to stereo by duplicating the channel
    fn convert_mono_to_stereo(samples: &[f32]) -> Vec<f32> {
        let mut stereo = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            stereo.push(sample);
            stereo.push(sample);
        }
        stereo
    }

    /// Convert PCM 16-bit signed integer data to 32-bit float samples
    fn convert_pcm_to_float(pcm_data: &[u8], format: &AudioFormat) -> Result<Vec<f32>, IronError> {
        // Reasonable upper bound for audio buffer size (10 seconds of 48kHz stereo audio)
        const MAX_REASONABLE_AUDIO_BUFFER_SIZE: usize = 48000 * 2 * 2 * 10; // ~1.9MB

        if pcm_data.len() > MAX_REASONABLE_AUDIO_BUFFER_SIZE {
            return Err(anyhow::Error::msg(format!(
                "audio buffer too large ({} bytes), possible malformed data (max: {} bytes)",
                pcm_data.len(),
                MAX_REASONABLE_AUDIO_BUFFER_SIZE
            ))
            .into());
        }

        if format.bits_per_sample != 16 {
            return Err(anyhow::Error::msg(format!(
                "unsupported bits per sample: {} (only 16-bit supported)",
                format.bits_per_sample
            ))
            .into());
        }

        if pcm_data.len() % 2 != 0 {
            return Err(anyhow::Error::msg("PCM data length must be even for 16-bit samples").into());
        }

        let mut float_samples = Vec::with_capacity(pcm_data.len() / 2);

        // Convert 16-bit signed PCM to float32 samples
        for chunk in pcm_data.chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            let float_sample = f32::from(sample) / 32768.0; // Convert to range [-1.0, 1.0]
            float_samples.push(float_sample);
        }

        Ok(float_samples)
    }

    /// Create an AudioBuffer from audio data
    /// Only handles PCM since that's what we advertise and what RDP servers typically send
    fn create_audio_buffer(&self, audio_data: &[u8], format: &AudioFormat) -> Result<AudioBuffer, IronError> {
        match format.format {
            WaveFormat::PCM => self.create_pcm_buffer(audio_data, format),
            _ => {
                // This should not happen since we only advertise PCM formats
                error!(
                    "Received unsupported audio format: {:?} - only PCM is supported",
                    format.format
                );
                Err(anyhow::Error::msg(format!("Unsupported audio format: {:?}", format.format)).into())
            }
        }
    }

    /// Create an AudioBuffer from PCM data with sample rate conversion
    fn create_pcm_buffer(&self, pcm_data: &[u8], format: &AudioFormat) -> Result<AudioBuffer, IronError> {
        let mut float_samples = Self::convert_pcm_to_float(pcm_data, format)?;

        // Convert sample rate if needed
        #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let context_rate = self.context_sample_rate.round() as u32;
        if format.n_samples_per_sec != context_rate {
            trace!(
                "Converting sample rate from {}Hz to {}Hz",
                format.n_samples_per_sec,
                context_rate
            );
            float_samples = Self::convert_sample_rate(
                &float_samples,
                format.n_samples_per_sec,
                context_rate,
                format.n_channels,
            );
        }

        // Convert channel count to stereo output
        let output_channels: u16 = 2;
        if format.n_channels == 1 {
            debug!("Converting mono to stereo");
            float_samples = Self::convert_mono_to_stereo(&float_samples);
        }
        // If format.n_channels == 2, no conversion needed.
        // We only advertise mono and stereo PCM, so other counts shouldn't arrive.

        let sample_count = float_samples.len() / output_channels as usize;

        let buffer = self
            .audio_context
            .create_buffer(
                output_channels.into(),
                u32::try_from(sample_count).map_err(|_| anyhow::Error::msg("sample count too large"))?,
                context_rate as f32,
            )
            .map_err(|e| anyhow::Error::msg(format!("failed to create Web Audio buffer: {e:?}")))?;

        // Deinterleave and copy into each channel.
        // NOTE: AudioBuffer::get_channel_data() returns a *copy* in wasm-bindgen,
        // so we must use copy_to_channel() to actually write into the buffer.
        for channel in 0..output_channels {
            let channel_data: Vec<f32> = float_samples
                .iter()
                .skip(channel as usize)
                .step_by(output_channels as usize)
                .copied()
                .collect();

            buffer
                .copy_to_channel(&channel_data, channel.into())
                .map_err(|e| anyhow::Error::msg(format!("failed to copy channel data: {e:?}")))?;
        }

        Ok(buffer)
    }

    /// Apply current volume to the gain node
    fn apply_volume(&self) {
        self.gain_node.gain().set_value(self.volume);
    }

    /// Check if the audio context is running and process any pending audio data.
    ///
    /// Returns `Ok(())` when the context is in the `Running` state.
    /// Returns `Err` when the context is still suspended (waiting for user gesture).
    fn try_resume_and_process_pending(&mut self) -> Result<(), IronError> {
        if self.audio_context.state() != web_sys::AudioContextState::Running {
            // Context still suspended; the user-gesture listener will call resume().
            return Err(anyhow::Error::msg("audio context suspended - user gesture required").into());
        }

        // Process any pending audio data that was buffered while suspended
        while let Some((data, format)) = self.pending_audio_data.pop_front() {
            if let Ok(buffer) = self.create_audio_buffer(&data, &format) {
                if let Err(e) = self.audio_queue.enqueue_audio(buffer, &self.gain_node) {
                    error!("Failed to enqueue pending audio: {e:?}");
                }
            }
        }

        Ok(())
    }

    /// Set up a one-time listener for user gestures to activate audio context.
    ///
    /// Browsers require a user gesture before allowing audio playback.
    /// Once the context transitions to `Running`, the listeners are removed.
    fn setup_user_gesture_listener(audio_context: AudioContext) {
        let window = match web_sys::window() {
            Some(w) => w,
            None => {
                warn!("No window object available for audio gesture activation");
                return;
            }
        };

        let document = match window.document() {
            Some(d) => d,
            None => {
                warn!("No document object available for audio gesture activation");
                return;
            }
        };

        type ClosureSlot = std::rc::Rc<core::cell::RefCell<Option<Closure<dyn FnMut(Event)>>>>;

        // We store the closure in an Rc so that it can remove itself from the
        // event listeners once the context is running, then drop itself.
        let closure_slot: ClosureSlot = std::rc::Rc::new(core::cell::RefCell::new(None));

        let closure_slot_inner = std::rc::Rc::clone(&closure_slot);
        let document_inner = document.clone();

        let closure = Closure::wrap(Box::new(move |_event: Event| {
            if audio_context.state() == web_sys::AudioContextState::Running {
                // Already running — just clean up.
            } else {
                // resume() returns a Promise; the context transitions asynchronously.
                // We don't need to track the promise — we'll check the state on next wave().
                let _ = audio_context.resume();
                info!("Audio context resume requested via user gesture");

                // Don't remove listeners yet — we'll be called again on the next
                // gesture if the context hasn't transitioned yet.
                if audio_context.state() != web_sys::AudioContextState::Running {
                    return;
                }
            }

            // Context is running — remove listeners and drop the closure.
            if let Some(handler) = closure_slot_inner.borrow_mut().take() {
                let callback = handler.as_ref().unchecked_ref();
                let _ = document_inner.remove_event_listener_with_callback("click", callback);
                let _ = document_inner.remove_event_listener_with_callback("keydown", callback);
                let _ = document_inner.remove_event_listener_with_callback("touchstart", callback);
                debug!("Audio gesture event listeners removed after activation");
            }
        }) as Box<dyn FnMut(Event)>);

        let callback_ref = closure.as_ref().unchecked_ref();
        if let Err(e) = document.add_event_listener_with_callback("click", callback_ref) {
            warn!("Failed to add click listener for audio activation: {e:?}");
        }
        if let Err(e) = document.add_event_listener_with_callback("keydown", callback_ref) {
            warn!("Failed to add keydown listener for audio activation: {e:?}");
        }
        if let Err(e) = document.add_event_listener_with_callback("touchstart", callback_ref) {
            warn!("Failed to add touchstart listener for audio activation: {e:?}");
        }

        info!("Audio gesture activation listeners registered (click, keydown, touchstart)");

        // Store the closure inside the slot to keep it alive.
        *closure_slot.borrow_mut() = Some(closure);
    }
}

impl AudioQueue {
    /// Enqueue an audio buffer for playback through the specified gain node
    fn enqueue_audio(&mut self, buffer: AudioBuffer, gain_node: &GainNode) -> Result<(), IronError> {
        // Validate audio context state
        match self.context.state() {
            web_sys::AudioContextState::Running => {}
            web_sys::AudioContextState::Suspended => {
                return Err(anyhow::Error::msg("Audio context suspended - user gesture required").into());
            }
            web_sys::AudioContextState::Closed => {
                return Err(anyhow::Error::msg("Audio context is closed").into());
            }
            _ => {
                return Err(anyhow::Error::msg("Audio context in unknown state").into());
            }
        }
        let source = self
            .context
            .create_buffer_source()
            .map_err(|e| anyhow::Error::msg(format!("Failed to create buffer source: {e:?}")))?;

        source.set_buffer(Some(&buffer));

        // Connect through the gain node for proper volume control
        source
            .connect_with_audio_node(gain_node)
            .map_err(|e| anyhow::Error::msg(format!("Failed to connect buffer source to gain node: {e:?}")))?;

        // Use the audio context's actual current time with proper scheduling
        let context_time = self.context.current_time();
        let start_time = context_time.max(self.current_time);

        source
            .start_with_when(start_time)
            .map_err(|e| anyhow::Error::msg(format!("Failed to start audio playback: {e:?}")))?;

        // Update tracking time for next buffer
        self.current_time = start_time + buffer.duration();

        trace!(
            "Audio buffer scheduled: duration={:.3}s, start_time={:.3}s, next_time={:.3}s, context_time={:.3}s",
            buffer.duration(),
            start_time,
            self.current_time,
            context_time
        );

        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
// SAFETY: This crate only targets `wasm32-unknown-unknown` which is single-threaded.
// `WebAudioBackend` contains non-Send JS handle types (`AudioContext`, `GainNode`,
// `Closure`) that are safe to move between Rust "threads" (futures tasks) in this
// single-threaded WASM execution environment. The `RdpsndClientHandler` trait
// requires `Send` because the same trait is used in native multi-threaded builds.
unsafe impl Send for WebAudioBackend {}

impl RdpsndClientHandler for WebAudioBackend {
    fn get_flags(&self) -> AudioFormatFlags {
        // Return basic flags for web audio compatibility
        AudioFormatFlags::empty()
    }

    fn get_formats(&self) -> &[AudioFormat] {
        &self.supported_formats
    }

    fn wave(&mut self, format_no: usize, ts: u32, data: Cow<'_, [u8]>) {
        trace!(
            "Received audio wave: format_no={}, timestamp={}, data_len={} bytes",
            format_no,
            ts,
            data.len()
        );

        if !self.is_active {
            debug!("Audio backend not active, ignoring wave data");
            return;
        }

        let format = match self.supported_formats.get(format_no) {
            Some(format) => format.clone(),
            None => {
                warn!("Invalid format number: {}", format_no);
                return;
            }
        };

        // Try to resume context and process audio
        if let Ok(()) = self.try_resume_and_process_pending() {
            // Context is ready, play audio immediately
            match self.create_audio_buffer(&data, &format) {
                Ok(buffer) => {
                    if let Err(e) = self.audio_queue.enqueue_audio(buffer, &self.gain_node) {
                        error!("Failed to enqueue audio: {:?}", e);
                    } else {
                        trace!("Successfully processed audio format: {:?}", format.format);
                    }
                }
                Err(e) => {
                    warn!("Failed to create audio buffer for format {:?}: {:?}", format.format, e);
                }
            }
        } else {
            // Context not ready, buffer the audio data
            if self.pending_audio_data.len() < 10 {
                // Limit buffer size
                self.pending_audio_data.push_back((data.to_vec(), format));
                debug!(
                    "Audio data buffered (context not ready), queue size: {}",
                    self.pending_audio_data.len()
                );
            } else {
                warn!("Audio buffer full, dropping oldest data (consider user gesture to activate audio)");
                self.pending_audio_data.pop_front();
                self.pending_audio_data.push_back((data.to_vec(), format));
            }
        }
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        debug!(
            "Setting volume: left={}, right={} (gain: {:.2})",
            volume.volume_left,
            volume.volume_right,
            (f32::from(volume.volume_left) + f32::from(volume.volume_right)) / (2.0 * 65535.0)
        );

        // Convert RDP volume (0-65535) to Web Audio gain (0.0-1.0)
        // For simplicity, use average of left and right channels
        let left_gain = f32::from(volume.volume_left) / 65535.0;
        let right_gain = f32::from(volume.volume_right) / 65535.0;
        self.volume = (left_gain + right_gain) / 2.0;

        self.apply_volume();
    }

    fn set_pitch(&mut self, pitch: PitchPdu) {
        debug!(
            "Setting pitch: {} (note: pitch control not implemented for web)",
            pitch.pitch
        );

        // Store pitch value but don't implement pitch shifting for now
        // Web Audio API pitch shifting would require more complex processing
        #[expect(clippy::cast_precision_loss)] // pitch range is 0..=65535, well within f32 precision
        {
            self.pitch = pitch.pitch.min(65535) as f32 / 65535.0;
        }
    }

    fn close(&mut self) {
        info!("Closing audio backend");
        self.is_active = false;

        // Suspend audio context to free resources
        if let Err(e) = self.audio_context.suspend() {
            warn!("Failed to suspend audio context: {e:?}");
        }

        // Audio queue cleanup (buffers are automatically cleaned up when context suspends)
        debug!("Audio backend closed, context suspended");
    }
}
