// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

use super::config::FRAME_LENGTH;
use super::tables::ELD_WINDOW_480;
use std::f64::consts::PI;

pub const N: usize = FRAME_LENGTH;
const M: usize = N / 2;
const RADICES: [usize; 4] = [4, 4, 3, 5];

#[derive(Clone, Copy, Default)]
struct Cpx {
    re: f32,
    im: f32,
}

impl Cpx {
    fn mul(self, o: Cpx) -> Cpx {
        Cpx {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }

    fn add(self, o: Cpx) -> Cpx {
        Cpx {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }
}

fn expi(angle: f64) -> Cpx {
    Cpx {
        re: angle.cos() as f32,
        im: angle.sin() as f32,
    }
}

// Half inverse MDCT of N coefficients (middle N samples of the 2N output) with
// the sign convention of libavutil's av_tx inverse MDCT, scaled by 1/N.
// Computed through a DCT-IV evaluated with an N/2-point complex FFT.
pub struct Imdct {
    roots: [Cpx; M],
    pre: [Cpx; M],
    post: [Cpx; M],
    a: [Cpx; M],
    b: [Cpx; M],
}

impl Imdct {
    pub fn new() -> Self {
        let mut roots = [Cpx::default(); M];
        let mut pre = [Cpx::default(); M];
        let mut post = [Cpx::default(); M];
        let scale = 1.0 / N as f64;
        for k in 0..M {
            roots[k] = expi(-2.0 * PI * k as f64 / M as f64);
            let p = expi(-PI * k as f64 / N as f64);
            pre[k] = p;
            let q = expi(-PI * (k as f64 + 0.25) / N as f64);
            post[k] = Cpx {
                re: (f64::from(q.re) * scale) as f32,
                im: (f64::from(q.im) * scale) as f32,
            };
        }
        Self {
            roots,
            pre,
            post,
            a: [Cpx::default(); M],
            b: [Cpx::default(); M],
        }
    }

    // Stockham autosort FFT of length M, input and output in `self.a`.
    fn fft(&mut self) {
        let mut n = M;
        let mut s = 1;
        let mut in_a = true;
        for &r in &RADICES {
            let m = n / r;
            let (src, dst) = if in_a {
                (&self.a, &mut self.b)
            } else {
                (&self.b, &mut self.a)
            };
            let tw_step = M / n;
            let root_step = M / r;
            for p in 0..m {
                for q in 0..s {
                    let mut x = [Cpx::default(); 5];
                    for (t, xt) in x.iter_mut().enumerate().take(r) {
                        *xt = src[q + s * (p + t * m)];
                    }
                    for u in 0..r {
                        let mut sum = x[0];
                        for (t, xt) in x.iter().enumerate().take(r).skip(1) {
                            sum = sum.add(xt.mul(self.roots[((t * u) % r) * root_step]));
                        }
                        if u > 0 {
                            sum = sum.mul(self.roots[p * u * tw_step]);
                        }
                        dst[q + s * (r * p + u)] = sum;
                    }
                }
            }
            n = m;
            s *= r;
            in_a = !in_a;
        }
        if !in_a {
            self.a = self.b;
        }
    }

    pub fn process(&mut self, x: &[f32; N], out: &mut [f32; N]) {
        for n in 0..M {
            let z = Cpx {
                re: x[2 * n],
                im: x[N - 1 - 2 * n],
            };
            self.a[n] = z.mul(self.pre[n]);
        }
        self.fft();
        for k in 0..M {
            let y = self.a[k].mul(self.post[k]);
            out[N - 1 - 2 * k] = y.re;
            out[2 * k] = -y.im;
        }
    }
}

pub struct EldFilterbank {
    saved: [f32; 3 * N],
    buf: [f32; N],
    imdct: Imdct,
}

impl EldFilterbank {
    pub fn new() -> Self {
        Self {
            saved: [0.0; 3 * N],
            buf: [0.0; N],
            imdct: Imdct::new(),
        }
    }

    // Consumes `coeffs` (permuted in place) and writes N output samples.
    pub fn process(&mut self, coeffs: &mut [f32; N], out: &mut [f32; N]) {
        let n = N;
        let n2 = n >> 1;
        let n4 = n >> 2;
        let w = &ELD_WINDOW_480;

        for i in (0..n2).step_by(2) {
            let t = coeffs[i];
            coeffs[i] = -coeffs[n - 1 - i];
            coeffs[n - 1 - i] = t;
            let t = -coeffs[i + 1];
            coeffs[i + 1] = coeffs[n - 2 - i];
            coeffs[n - 2 - i] = t;
        }

        self.imdct.process(coeffs, &mut self.buf);

        for i in (0..n).step_by(2) {
            self.buf[i] = -self.buf[i];
        }

        let buf = &self.buf;
        let saved = &self.saved;
        for i in n4..n2 {
            out[i - n4] = buf[n2 - 1 - i] * w[i - n4]
                + saved[i + n2] * w[i + n - n4]
                + -saved[n + n2 - 1 - i] * w[i + 2 * n - n4]
                + -saved[2 * n + n2 + i] * w[i + 3 * n - n4];
        }
        for i in 0..n2 {
            out[n4 + i] = buf[i] * w[i + n2 - n4]
                + -saved[n - 1 - i] * w[i + n2 + n - n4]
                + -saved[n + i] * w[i + n2 + 2 * n - n4]
                + saved[2 * n + n - 1 - i] * w[i + n2 + 3 * n - n4];
        }
        for i in 0..n4 {
            out[n2 + n4 + i] = buf[i + n2] * w[i + n - n4]
                + -saved[n2 - 1 - i] * w[i + 2 * n - n4]
                + -saved[n + n2 + i] * w[i + 3 * n - n4];
        }

        self.saved.copy_within(0..2 * n, n);
        self.saved[..n].copy_from_slice(&self.buf);
    }
}
