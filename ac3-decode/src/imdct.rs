//! 256-point IMDCT for AC-3, ported from ac3.js mdct.js (MIT).
//!
//! Uses 128-point complex FFT internally.
//! Window function from precomputed WINDOW table (not calculated).

use crate::tables::WINDOW;
use std::f64::consts::PI;

const N: usize = 512;
const N2: usize = N / 2; // 256
const N4: usize = N / 4; // 128
const N8: usize = N / 8; // 64

/// IMDCT processor with per-channel delay buffer.
pub struct Imdct {
    /// Pre/post twiddle factors: xcos1[i] = -cos(2π(8i+1)/4096), xsin1[i] = -sin(...)
    xcos1: [f32; N4],
    xsin1: [f32; N4],
    /// Delay samples for overlap-add (second half of previous output).
    delay: [f32; N2],
}

impl Imdct {
    pub fn new() -> Self {
        let mut xcos1 = [0.0f32; N4];
        let mut xsin1 = [0.0f32; N4];

        for i in 0..N4 {
            let angle = 2.0 * PI * (8.0 * i as f64 + 1.0) / (8.0 * N as f64);
            xcos1[i] = -(angle.cos() as f32);
            xsin1[i] = -(angle.sin() as f32);
        }

        Self {
            xcos1,
            xsin1,
            delay: [0.0; N2],
        }
    }

    /// Process 256 frequency-domain coefficients into 256 time-domain PCM samples.
    /// Implements pre-twiddle → 128-point inverse FFT → post-twiddle → window → overlap-add.
    /// Output is written to `output[offset..offset+256]`.
    /// Ported from ac3.js IMDCT.process256().
    pub fn process256(&mut self, coeffs: &[f32], output: &mut [f32], offset: usize) {
        // Step 1: Pre-twiddle — form 128 complex values
        let mut z_re = [0.0f32; N4];
        let mut z_im = [0.0f32; N4];

        for k in 0..N4 {
            // Z[k] = coeffs[N/2-1-2k] * xcos1[k] - coeffs[2k] * xsin1[k]  (real)
            //      + j*(coeffs[2k] * xcos1[k] + coeffs[N/2-1-2k] * xsin1[k])  (imag)
            let c_even = if 2 * k < coeffs.len() {
                coeffs[2 * k]
            } else {
                0.0
            };
            let c_odd = if N2 - 1 - 2 * k < coeffs.len() {
                coeffs[N2 - 1 - 2 * k]
            } else {
                0.0
            };

            z_re[k] = c_odd * self.xcos1[k] - c_even * self.xsin1[k];
            z_im[k] = c_even * self.xcos1[k] + c_odd * self.xsin1[k];
        }

        // Step 2: 128-point inverse complex FFT
        Self::ifft128(&mut z_re, &mut z_im);

        // Step 3: Post-twiddle
        let mut y_re = [0.0f32; N4];
        let mut y_im = [0.0f32; N4];
        for n in 0..N4 {
            y_re[n] = z_re[n] * self.xcos1[n] - z_im[n] * self.xsin1[n];
            y_im[n] = z_im[n] * self.xcos1[n] + z_re[n] * self.xsin1[n];
        }

        // Step 4: Window and interleave into x256[512]
        let mut x256 = [0.0f32; N];
        for n in 0..N8 {
            // Quadrant 1: x[0..N/4]
            x256[2 * n] = -y_im[N8 + n] * WINDOW[2 * n];
            x256[2 * n + 1] = y_re[N8 - n - 1] * WINDOW[2 * n + 1];

            // Quadrant 2: x[N/4..N/2]
            x256[N4 + 2 * n] = -y_re[n] * WINDOW[N4 + 2 * n];
            x256[N4 + 2 * n + 1] = y_im[N4 - n - 1] * WINDOW[N4 + 2 * n + 1];

            // Quadrant 3: x[N/2..3N/4]
            x256[N2 + 2 * n] = -y_re[N8 + n] * WINDOW[N2 - 2 * n - 1];
            x256[N2 + 2 * n + 1] = y_im[N8 - n - 1] * WINDOW[N2 - 2 * n - 2];

            // Quadrant 4: x[3N/4..N]
            x256[3 * N4 + 2 * n] = y_im[n] * WINDOW[N4 - 2 * n - 1];
            x256[3 * N4 + 2 * n + 1] = -y_re[N4 - n - 1] * WINDOW[N4 - 2 * n - 2];
        }

        // Step 5: Overlap-add with delay, scale by 128, clip to [-1, 1]
        for n in 0..N2 {
            let mut sample = 128.0 * (x256[n] + self.delay[n]);
            if sample < -1.0 {
                sample = -1.0;
            } else if sample > 1.0 {
                sample = 1.0;
            }
            output[n + offset] = sample;
            self.delay[n] = x256[N2 + n];
        }
    }

    /// Reset delay buffer (e.g., after seek).
    pub fn reset(&mut self) {
        self.delay = [0.0; N2];
    }

    /// 128-point inverse complex FFT (in-place, radix-2 DIT).
    /// Input/output as separate real and imaginary arrays.
    fn ifft128(re: &mut [f32; N4], im: &mut [f32; N4]) {
        let n = N4; // 128

        // Conjugate input (inverse FFT = conjugate → FFT → conjugate → scale)
        for i in 0..n {
            im[i] = -im[i];
        }

        // Bit-reversal permutation
        let mut j = 0usize;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j ^= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }

        // Cooley-Tukey butterfly
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let angle_step = -2.0 * std::f32::consts::PI / len as f32;

            for i in (0..n).step_by(len) {
                for k in 0..half {
                    let angle = angle_step * k as f32;
                    let tw_re = angle.cos();
                    let tw_im = angle.sin();

                    let u_re = re[i + k];
                    let u_im = im[i + k];
                    let v_re = re[i + k + half];
                    let v_im = im[i + k + half];

                    let t_re = v_re * tw_re - v_im * tw_im;
                    let t_im = v_re * tw_im + v_im * tw_re;

                    re[i + k] = u_re + t_re;
                    im[i + k] = u_im + t_im;
                    re[i + k + half] = u_re - t_re;
                    im[i + k + half] = u_im - t_im;
                }
            }
            len <<= 1;
        }

        // Conjugate and scale (1/N for inverse FFT)
        let scale = 1.0 / n as f32;
        for i in 0..n {
            re[i] *= scale;
            im[i] = -im[i] * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imdct_dc_input() {
        let mut imdct = Imdct::new();
        let mut coeffs = [0.0f32; 256];
        coeffs[0] = 1.0;
        let mut output = [0.0f32; 256];
        imdct.process256(&coeffs, &mut output, 0);

        // Output should not be all zeros
        let energy: f32 = output.iter().map(|x| x * x).sum();
        assert!(energy > 0.0, "IMDCT output is all zeros");
    }

    #[test]
    fn imdct_output_bounded() {
        let mut imdct = Imdct::new();
        let mut coeffs = [0.0f32; 256];
        coeffs[0] = 1.0;
        coeffs[10] = 0.5;
        let mut output = [0.0f32; 256];
        imdct.process256(&coeffs, &mut output, 0);

        // All outputs should be in [-1, 1] due to clipping
        for (i, &s) in output.iter().enumerate() {
            assert!(s >= -1.0 && s <= 1.0, "Sample {} out of range: {}", i, s);
        }
    }

    #[test]
    fn ifft_impulse() {
        let mut re = [0.0f32; 128];
        let mut im = [0.0f32; 128];
        re[0] = 1.0; // DC impulse
        Imdct::ifft128(&mut re, &mut im);
        // IFFT of [1, 0, 0, ...] should be [1/128, 1/128, ...]
        let expected = 1.0 / 128.0;
        for k in 0..128 {
            assert!(
                (re[k] - expected).abs() < 1e-5,
                "IFFT[{}] = {} expected {}",
                k,
                re[k],
                expected
            );
        }
    }
}
