use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait as _, HostTrait as _};
use cpal::{SampleFormat, Stream, StreamConfig};
use ironrdp_error::bail;
use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use tracing::{debug, error, trace, warn};

use crate::error::{RdpsndNativeError, RdpsndNativeErrorKind, RdpsndNativeResult};

#[derive(Debug)]
pub struct RdpsndBackend {
    // Unfortunately, Stream is not `Send`, so we move it to a separate thread.
    stream_handle: Option<JoinHandle<()>>,
    stream_ended: Arc<AtomicBool>,
    tx: Option<Sender<Vec<u8>>>,
    format_no: Option<usize>,
}

impl Default for RdpsndBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RdpsndBackend {
    pub fn new() -> Self {
        Self {
            tx: None,
            format_no: None,
            stream_handle: None,
            stream_ended: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for RdpsndBackend {
    fn drop(&mut self) {
        self.close();
    }
}

impl RdpsndClientHandler for RdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &[
            #[cfg(feature = "opus")]
            AudioFormat {
                format: WaveFormat::OPUS,
                n_channels: 2,
                n_samples_per_sec: 48000,
                n_avg_bytes_per_sec: 192000,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            },
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: 44100,
                n_avg_bytes_per_sec: 176400,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            },
        ]
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: Cow<'_, [u8]>) {
        if Some(format_no) != self.format_no {
            debug!("New audio format");
            self.close();
        }

        if self.stream_handle.is_none() {
            let (tx, rx) = mpsc::channel();
            self.tx = Some(tx);

            self.format_no = Some(format_no);
            let Some(format) = self.get_formats().get(format_no) else {
                warn!(?format_no, "Invalid format_no");
                return;
            };
            let format = format.clone();
            self.stream_ended.store(false, Ordering::Relaxed);
            let stream_ended = Arc::clone(&self.stream_ended);
            self.stream_handle = Some(thread::spawn(move || {
                let stream = match DecodeStream::new(&format, rx) {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!(error = %e.report());
                        return;
                    }
                };
                debug!("Stream thread parking loop");
                while !stream_ended.load(Ordering::Relaxed) {
                    thread::park();
                }
                debug!("Stream thread unparked");
                drop(stream);
            }));
        }

        if let Some(ref tx) = self.tx {
            if let Err(error) = tx.send(data.to_vec()) {
                error!(%error);
            }
        };
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        debug!(?volume);
    }

    fn set_pitch(&mut self, pitch: PitchPdu) {
        debug!(?pitch);
    }

    fn close(&mut self) {
        self.tx = None;
        if let Some(stream) = self.stream_handle.take() {
            self.stream_ended.store(true, Ordering::Relaxed);
            stream.thread().unpark();
            if let Err(err) = stream.join() {
                error!(?err, "Failed to join a stream thread");
            }
        }
    }
}

#[doc(hidden)]
pub struct DecodeStream {
    _dec_thread: Option<JoinHandle<()>>,
    stream: Stream,
}

impl DecodeStream {
    #[cfg_attr(not(feature = "opus"), expect(unused_mut, reason = "reassigned only for Opus"))]
    pub fn new(rx_format: &AudioFormat, mut rx: Receiver<Vec<u8>>) -> RdpsndNativeResult<Self> {
        let mut dec_thread = None;
        match rx_format.format {
            #[cfg(feature = "opus")]
            WaveFormat::OPUS => {
                let chan = match rx_format.n_channels {
                    1 => opus2::Channels::Mono,
                    2 => opus2::Channels::Stereo,
                    _ => bail!(
                        "unsupported channel count for Opus",
                        RdpsndNativeErrorKind::UnsupportedFormat,
                    ),
                };
                let (dec_tx, dec_rx) = mpsc::channel();
                let mut dec = opus2::Decoder::new(rx_format.n_samples_per_sec, chan).map_err(|e| {
                    RdpsndNativeError::new("creating Opus decoder", RdpsndNativeErrorKind::OpusInit).with_source(e)
                })?;
                dec_thread = Some(thread::spawn(move || {
                    while let Ok(pkt) = rx.recv() {
                        let nb_samples = match dec.get_nb_samples(&pkt) {
                            Ok(nb_samples) => nb_samples,
                            Err(error) => {
                                error!(?error, "Failed to get the number of samples of an Opus packet");
                                continue;
                            }
                        };

                        #[expect(
                            clippy::as_conversions,
                            reason = "opus::Channels has no conversions to usize implemented"
                        )]
                        let mut pcm_i16 = vec![0i16; nb_samples * chan as usize];
                        if let Err(error) = dec.decode(&pkt, &mut pcm_i16, false) {
                            error!(?error, "Failed to decode an Opus packet");
                            continue;
                        }
                        // Vec<u8> is what the channel carries downstream. Reinterpreting
                        // Vec<i16> -> Vec<u8> via cast_slice is safe (smaller alignment).
                        // Allocating as Vec<i16> in the first place avoids the alignment
                        // hazard of `bytemuck::cast_slice_mut::<u8, i16>` panicking when
                        // the allocator hands back a u8 buffer that is not 2-byte aligned
                        // (which manifested as a hard crash in #1202 under the burst of
                        // malformed Opus packets generated by a server reboot).
                        let pcm = bytemuck::cast_slice(&pcm_i16).to_vec();

                        if dec_tx.send(pcm).is_err() {
                            error!("Failed to send the decoded Opus packet over the channel");
                            // If send has failed, it means that the receiver has been dropped.
                            // There is no point in continuing the loop in this case.
                            break;
                        }
                    }
                }));
                rx = dec_rx;
            }
            WaveFormat::PCM => {}
            _ => bail!(
                "matching server-requested wave format",
                RdpsndNativeErrorKind::UnsupportedFormat,
            ),
        }

        let sample_format = match rx_format.bits_per_sample {
            8 => SampleFormat::U8,
            16 => SampleFormat::I16,
            _ => bail!(
                "only PCM 8/16 bit formats supported",
                RdpsndNativeErrorKind::UnsupportedFormat,
            ),
        };

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| RdpsndNativeError::new("no default output device", RdpsndNativeErrorKind::AudioDevice))?;
        let _supported_configs_range = device.supported_output_configs().map_err(|e| {
            RdpsndNativeError::new("no supported output configs", RdpsndNativeErrorKind::AudioDevice).with_source(e)
        })?;
        let default_config = device.default_output_config().map_err(|e| {
            RdpsndNativeError::new("default output config", RdpsndNativeErrorKind::AudioDevice).with_source(e)
        })?;
        debug!(?default_config);

        let mut rx = RxBuffer::new(rx, rx_format);
        let config = StreamConfig {
            channels: rx_format.n_channels,
            sample_rate: rx_format.n_samples_per_sec,
            buffer_size: cpal::BufferSize::Default,
        };
        debug!(?config);

        let stream = device
            .build_output_stream_raw(
                &config,
                sample_format,
                move |data, _info: &cpal::OutputCallbackInfo| {
                    let data = data.bytes_mut();
                    rx.fill(data)
                },
                |error| error!(%error),
                None,
            )
            .map_err(|e| {
                RdpsndNativeError::new("building cpal output stream", RdpsndNativeErrorKind::StreamBuild).with_source(e)
            })?;

        Ok(Self {
            _dec_thread: dec_thread,
            stream,
        })
    }

    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}

/// How much audio to collect before playing, so network jitter doesn't starve the device.
const PREBUFFER: Duration = Duration::from_millis(80);
/// Beyond this much queued audio, the oldest is dropped so playback doesn't drift behind.
const MAX_BUFFERED: Duration = Duration::from_millis(300);

/// Feeds the audio device. [`fill`](Self::fill) runs on the real-time audio thread, so it never
/// blocks: when the next packet is late it plays silence, then collects [`PREBUFFER`] again
/// before resuming.
struct RxBuffer {
    receiver: Receiver<Vec<u8>>,
    queue: VecDeque<u8>,
    playing: bool,
    silence: u8,
    block_align: usize,
    prebuffer: usize,
    max_buffered: usize,
}

impl RxBuffer {
    fn new(receiver: Receiver<Vec<u8>>, format: &AudioFormat) -> Self {
        let block_align = usize::from(format.n_block_align.max(1));
        let bytes = |duration: Duration| {
            let bytes = u128::from(format.n_avg_bytes_per_sec) * duration.as_millis() / 1000;
            let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
            bytes - bytes % block_align
        };
        let max_buffered = bytes(MAX_BUFFERED);
        Self {
            receiver,
            queue: VecDeque::with_capacity(max_buffered.saturating_mul(2)),
            playing: false,
            // 8-bit PCM is unsigned, so its silence is the midpoint.
            silence: if format.bits_per_sample == 8 { 0x80 } else { 0 },
            block_align,
            prebuffer: bytes(PREBUFFER),
            max_buffered,
        }
    }

    fn fill(&mut self, data: &mut [u8]) {
        while let Ok(packet) = self.receiver.try_recv() {
            self.queue.extend(packet);
        }

        if self.queue.len() > self.max_buffered {
            let excess = self.queue.len() - self.prebuffer;
            let excess = excess - excess % self.block_align;
            self.queue.drain(..excess);
            debug!(dropped = excess, "Playback fell behind; dropping queued audio");
        }

        if !self.playing {
            if self.queue.len() < self.prebuffer {
                data.fill(self.silence);
                return;
            }
            self.playing = true;
        }

        let available = self.queue.len().min(data.len());
        for (dst, src) in data.iter_mut().zip(self.queue.drain(..available)) {
            *dst = src;
        }
        if available < data.len() {
            data[available..].fill(self.silence);
            self.playing = false;
            trace!("Playback rx underrun");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 kB per millisecond, 4-byte frames: easy arithmetic.
    fn buffer() -> (Sender<Vec<u8>>, RxBuffer) {
        let format = AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 250_000,
            n_avg_bytes_per_sec: 1_000_000,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        };
        let (tx, rx) = mpsc::channel();
        (tx, RxBuffer::new(rx, &format))
    }

    #[test]
    fn plays_silence_until_prebuffered() {
        let (tx, mut rx) = buffer();
        tx.send(vec![7; 40_000]).unwrap();
        let mut out = vec![1; 1000];
        rx.fill(&mut out);
        assert!(out.iter().all(|&b| b == 0), "silence while under 80 ms");

        tx.send(vec![7; 40_000]).unwrap();
        rx.fill(&mut out);
        assert!(out.iter().all(|&b| b == 7), "plays once 80 ms are queued");
    }

    #[test]
    fn underrun_fills_silence_and_prebuffers_again() {
        let (tx, mut rx) = buffer();
        tx.send(vec![7; 80_000]).unwrap();
        let mut out = vec![1; 79_000];
        rx.fill(&mut out);

        let mut out = vec![1; 2000];
        rx.fill(&mut out);
        assert!(out[..1000].iter().all(|&b| b == 7));
        assert!(out[1000..].iter().all(|&b| b == 0));

        tx.send(vec![7; 1000]).unwrap();
        rx.fill(&mut out);
        assert!(out.iter().all(|&b| b == 0), "waits for 80 ms again");
    }

    #[test]
    fn never_blocks_when_the_server_stops() {
        let (tx, mut rx) = buffer();
        drop(tx);
        let mut out = vec![1; 1000];
        rx.fill(&mut out);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn drops_old_audio_beyond_the_limit_on_frame_boundaries() {
        let (tx, mut rx) = buffer();
        tx.send(vec![1; 300_000]).unwrap();
        tx.send(vec![2; 10_002 * 4]).unwrap();
        let mut out = vec![0; 4];
        rx.fill(&mut out);
        assert_eq!(rx.queue.len() % 4, 0);
        assert!(rx.queue.len() <= 80_000);
        assert_eq!(out, [1; 4]);
    }
}
