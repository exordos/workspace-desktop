//! Воспроизводит локальные звуки Workspace через AppKit. Тоны совпадают с настройками Electron.

use std::{cell::RefCell, f32::consts::TAU};

use objc2::{ClassType, rc::Retained};
use objc2_app_kit::NSSound;
use objc2_foundation::NSData;

use super::preferences::NotificationSound;

const SAMPLE_RATE: u32 = 44_100;

thread_local! {
    static PLAYING: RefCell<Option<Retained<NSSound>>> = const { RefCell::new(None) };
}

/// Составляющая короткого сигнала, которую можно наложить на другие тоны.
struct Tone {
    frequency: f32, // Герцы.
    gain: f32,      // Начальная амплитуда от нуля до единицы.
    length: f32,    // Длительность в секундах.
    offset: f32,    // Задержка начала от начала сигнала в секундах.
    wave: Wave,     // Форма звуковой волны.
}

/// Формы волн Web Audio, используемые пресетами.
#[derive(Clone, Copy)]
enum Wave {
    Sine,     // Синусоида.
    Triangle, // Треугольная волна.
    Square,   // Прямоугольная волна.
}

/// Заменяет текущий звук, сохраняя объект AppKit на время воспроизведения.
pub(super) fn preview(preset: NotificationSound) {
    PLAYING.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(previous) = slot.take() {
            // Этот адаптер вызывается из событий GPUI на главном потоке.
            unsafe { previous.stop() };
        }
        let tones = tones(preset);
        if tones.is_empty() {
            return;
        }
        let data = NSData::from_vec(pcm_wav(tones));
        if let Some(sound) = unsafe { NSSound::initWithData(NSSound::alloc(), &data) }
            && unsafe { sound.play() }
        {
            *slot = Some(sound);
        }
    });
}

/// Возвращает параметры звуковых волн из Electron.
fn tones(preset: NotificationSound) -> &'static [Tone] {
    use Wave::{Sine, Square, Triangle};
    match preset {
        NotificationSound::Default => &[Tone {
            frequency: 800.,
            gain: 0.1,
            length: 0.3,
            offset: 0.,
            wave: Sine,
        }],
        NotificationSound::Subtle => &[Tone {
            frequency: 660.,
            gain: 0.07,
            length: 0.22,
            offset: 0.,
            wave: Sine,
        }],
        NotificationSound::Digital => &[
            Tone {
                frequency: 880.,
                gain: 0.09,
                length: 0.12,
                offset: 0.,
                wave: Triangle,
            },
            Tone {
                frequency: 1320.,
                gain: 0.08,
                length: 0.12,
                offset: 0.09,
                wave: Triangle,
            },
        ],
        NotificationSound::Glass => &[Tone {
            frequency: 1480.,
            gain: 0.075,
            length: 0.18,
            offset: 0.,
            wave: Sine,
        }],
        NotificationSound::Pulse => &[
            Tone {
                frequency: 640.,
                gain: 0.09,
                length: 0.1,
                offset: 0.,
                wave: Square,
            },
            Tone {
                frequency: 760.,
                gain: 0.085,
                length: 0.1,
                offset: 0.14,
                wave: Square,
            },
        ],
        NotificationSound::None => &[],
    }
}

/// Синтезирует тон в несжатом моно WAV без файлов на диске.
fn pcm_wav(tones: &[Tone]) -> Vec<u8> {
    let duration = tones
        .iter()
        .map(|tone| tone.offset + tone.length)
        .fold(0., f32::max);
    let sample_count = (duration * SAMPLE_RATE as f32).ceil() as u32;
    let payload = sample_count * 2;
    let mut wav = Vec::with_capacity(44 + payload as usize);
    wav.extend(b"RIFF");
    wav.extend((36 + payload).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes()); // Размер PCM-заголовка.
    wav.extend(1_u16.to_le_bytes()); // PCM.
    wav.extend(1_u16.to_le_bytes()); // Один канал.
    wav.extend(SAMPLE_RATE.to_le_bytes());
    wav.extend((SAMPLE_RATE * 2).to_le_bytes());
    wav.extend(2_u16.to_le_bytes()); // Два байта в отсчёте.
    wav.extend(16_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(payload.to_le_bytes());
    for sample in 0..sample_count {
        let time = sample as f32 / SAMPLE_RATE as f32;
        let level: f32 = tones.iter().map(|tone| tone.sample(time)).sum();
        wav.extend(((level * f32::from(i16::MAX)) as i16).to_le_bytes());
    }
    wav
}

impl Tone {
    /// Возвращает один отсчёт с экспоненциальным спадом громкости.
    fn sample(&self, time: f32) -> f32 {
        let elapsed = time - self.offset;
        if elapsed < 0. || elapsed >= self.length {
            return 0.;
        }
        let phase = (elapsed * self.frequency).fract();
        let signal = match self.wave {
            Wave::Sine => (phase * TAU).sin(),
            Wave::Triangle => 4. * (phase - (phase + 0.5).floor()).abs() - 1.,
            Wave::Square => {
                if phase < 0.5 {
                    1.
                } else {
                    -1.
                }
            }
        };
        let gain = self.gain * (0.001 / self.gain).powf(elapsed / self.length);
        signal * gain
    }
}
