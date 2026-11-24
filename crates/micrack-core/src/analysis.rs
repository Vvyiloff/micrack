//! Offline voice analysis for agents: loudness/dynamics/sibilance metrics plus
//! concrete starting points for compression, de-essing, EQ and makeup gain.
//! Runs on the control thread (or any caller) — never on the audio thread.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

const FFT_LEN: usize = 2048;
const TARGET_LUFS: f64 = -16.0;

#[derive(Debug, Clone, Serialize)]
pub struct VoiceAnalysis {
    pub lufs_integrated: f64,
    pub lra: f64,
    pub true_peak: f64,
    pub peak: f64,
    pub rms: f64,
    /// peak / rms (linear; 20*log10 of it is the crest in dB).
    pub crest_factor: f64,
    /// 95th percentile of per-frame energy(4-10 kHz) / energy(total).
    pub sibilance_ratio: f64,
    pub sibilance_freq_hz: f64,
    pub spectral_centroid_hz: f64,
    pub spectral_tilt_db_per_oct: f64,
    pub recommendations: Vec<String>,
}

/// Read any WAV hound supports and downmix to mono f32.
fn read_mono(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, _) => r.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        (_, b) if b <= 16 => r.samples::<i16>().map(|s| s.unwrap_or(0) as f32 / 32768.0).collect(),
        _ => r.samples::<i32>().map(|s| s.unwrap_or(0) as f32 / 2_147_483_648.0).collect(),
    };
    let mono: Vec<f32> = if ch == 1 {
        interleaved
    } else {
        interleaved.chunks(ch).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect()
    };
    if mono.len() < FFT_LEN {
        bail!("wav too short to analyze: {} frames (need {FFT_LEN})", mono.len());
    }
    Ok((mono, spec.sample_rate))
}

pub fn analyze_voice(wav_path: &Path) -> Result<VoiceAnalysis> {
    let (mono, rate) = read_mono(wav_path)?;

    let mut ebu = ebur128::EbuR128::new(1, rate, ebur128::Mode::I | ebur128::Mode::LRA | ebur128::Mode::TRUE_PEAK)
        .map_err(|e| anyhow::anyhow!("ebur128 init: {e:?}"))?;
    ebu.add_frames_f32(&mono).map_err(|e| anyhow::anyhow!("ebur128 add_frames: {e:?}"))?;
    let finite = |x: f64, fallback: f64| if x.is_finite() { x } else { fallback };
    let lufs = finite(ebu.loudness_global().unwrap_or(-70.0), -70.0);
    let lra = finite(ebu.loudness_range().unwrap_or(0.0), 0.0);

    let peak = mono.iter().fold(0f32, |p, s| p.max(s.abs())) as f64;
    let rms = (mono.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / mono.len() as f64).sqrt();
    let true_peak = finite(ebu.true_peak(0).unwrap_or(peak), peak);
    let crest = if rms > 0.0 { peak / rms } else { 0.0 };

    // --- spectral: 2048-sample Hann frames, 50% overlap ---
    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let mut inbuf = fft.make_input_vec();
    let mut outbuf = fft.make_output_vec();
    let bins = outbuf.len();
    let bin_hz = rate as f64 / FFT_LEN as f64;
    let hann: Vec<f32> = (0..FFT_LEN)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (FFT_LEN - 1) as f32).cos()))
        .collect();
    let band = |lo_hz: f64, hi_hz: f64| {
        let lo = (lo_hz / bin_hz).ceil() as usize;
        let hi = ((hi_hz / bin_hz) as usize).min(bins - 1);
        (lo.min(bins - 1), hi.max(lo.min(bins - 1)))
    };
    let (sib_lo, sib_hi) = band(4000.0, 10000.0);
    let (dom_lo, dom_hi) = band(5000.0, 9000.0);

    let mut mean_mag = vec![0f64; bins];
    let mut ratios: Vec<f64> = Vec::new();
    let mut dom_bands: Vec<Vec<f64>> = Vec::new();
    let mut pos = 0;
    while pos + FFT_LEN <= mono.len() {
        for i in 0..FFT_LEN {
            inbuf[i] = mono[pos + i] * hann[i];
        }
        fft.process(&mut inbuf, &mut outbuf).context("fft process")?;
        let (mut total, mut sib) = (0f64, 0f64);
        let mut dom = vec![0f64; dom_hi - dom_lo + 1];
        for (k, c) in outbuf.iter().enumerate() {
            let m = ((c.re * c.re + c.im * c.im) as f64).sqrt();
            mean_mag[k] += m;
            total += m * m;
            if k >= sib_lo && k <= sib_hi {
                sib += m * m;
            }
            if k >= dom_lo && k <= dom_hi {
                dom[k - dom_lo] = m;
            }
        }
        ratios.push(if total > 0.0 { sib / total } else { 0.0 });
        dom_bands.push(dom);
        pos += FFT_LEN / 2;
    }
    let frames = ratios.len().max(1) as f64;
    for m in &mut mean_mag {
        *m /= frames;
    }

    // Sibilants are bursts: take the 95th percentile, not the mean.
    let mut sorted = ratios.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_idx = ((sorted.len() - 1) as f64 * 0.95).round() as usize;
    let sibilance_ratio = sorted[p95_idx].clamp(0.0, 1.0);
    let mut dom_sum = vec![0f64; dom_hi - dom_lo + 1];
    for (i, r) in ratios.iter().enumerate() {
        if *r >= sibilance_ratio {
            for (k, m) in dom_bands[i].iter().enumerate() {
                dom_sum[k] += m;
            }
        }
    }
    let dom_peak = dom_sum.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal));
    let sibilance_freq_hz = dom_peak.map(|(k, _)| (dom_lo + k) as f64 * bin_hz).unwrap_or(0.0);

    let mag_sum: f64 = mean_mag.iter().skip(1).sum();
    let centroid = if mag_sum > 0.0 {
        mean_mag.iter().enumerate().skip(1).map(|(k, m)| k as f64 * bin_hz * m).sum::<f64>() / mag_sum
    } else {
        0.0
    };

    // Tilt: least-squares slope of dB against log2(f) over 100 Hz..10 kHz.
    let (mut sx, mut sy, mut sxx, mut sxy, mut cnt) = (0f64, 0f64, 0f64, 0f64, 0f64);
    for (k, m) in mean_mag.iter().enumerate().skip(1) {
        let f = k as f64 * bin_hz;
        if !(100.0..=10000.0).contains(&f) {
            continue;
        }
        let x = f.log2();
        let y = 20.0 * (m + 1e-12).log10();
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
        cnt += 1.0;
    }
    let den = cnt * sxx - sx * sx;
    let tilt = if den.abs() > 1e-9 { (cnt * sxy - sx * sy) / den } else { 0.0 };

    let mut recommendations = Vec::new();
    let crest_db = 20.0 * crest.max(1e-9).log10();
    let rms_db = 20.0 * rms.max(1e-9).log10();
    if crest_db > 12.0 || lra > 8.0 {
        recommendations.push(format!(
            "compressor: threshold ~{:.0} dB, ratio ~{:.0}:1, attack ~10 ms, release ~120 ms (crest {crest_db:.1} dB, LRA {lra:.1} LU)",
            rms_db + 6.0,
            if crest_db > 18.0 { 4.0 } else { 3.0 }
        ));
    }
    if sibilance_ratio > 0.20 && sibilance_freq_hz > 0.0 {
        recommendations.push(format!(
            "de-esser: frequency ~{sibilance_freq_hz:.0} Hz, threshold ~{:.0} dB, reduction ~4 dB (sibilance {:.0}% of frame energy)",
            rms_db - 6.0,
            sibilance_ratio * 100.0
        ));
    }
    if tilt < -12.0 {
        recommendations.push(format!("EQ tilt: high shelf +{:.1} dB above 2 kHz (tilt {tilt:.1} dB/oct, dark)", (-tilt - 9.0).min(6.0)));
    } else if tilt > -3.0 {
        recommendations.push(format!("EQ tilt: high shelf -3.0 dB above 2 kHz (tilt {tilt:.1} dB/oct, bright)"));
    }
    let makeup = (TARGET_LUFS - lufs).clamp(-24.0, 24.0);
    if makeup.abs() > 1.0 {
        recommendations.push(format!("makeup gain ~{makeup:+.1} dB to reach {TARGET_LUFS:.0} LUFS (now {lufs:.1} LUFS)"));
    }
    if true_peak > 0.95 {
        recommendations.push("true peak above 0.95: lower master gain, keep the output ceiling on".into());
    }

    Ok(VoiceAnalysis {
        lufs_integrated: lufs,
        lra,
        true_peak,
        peak,
        rms,
        crest_factor: crest,
        sibilance_ratio,
        sibilance_freq_hz,
        spectral_centroid_hz: centroid,
        spectral_tilt_db_per_oct: tilt,
        recommendations,
    })
}