//! Portable seeded R7 A2 distribution checks.

#[cfg(windows)]
#[path = "m7c/windows_cases.rs"]
mod windows_cases;

#[derive(Clone, Copy)]
enum A2Shape {
    Plain(usize),
    Zstd(usize),
}

struct R7Sampler {
    state: u64,
}

impl R7Sampler {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn unit(&mut self) -> f64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.state >> 11) as f64 / ((1u64 << 53) as f64)
    }

    fn next(&mut self) -> (&'static str, A2Shape) {
        let p = self.unit();
        let size = if p < 0.35 {
            800 * 1024
        } else if p < 0.50 {
            1_400 * 1024
        } else if p < 0.90 {
            6 * 1024 * 1024
        } else if p < 0.97 {
            33 * 1024 * 1024
        } else {
            51 * 1024 * 1024
        };
        let compressed = size >= 33 * 1024 * 1024 && self.unit() < 0.35;
        let bucket = if size < 1024 * 1024 {
            "<1 MB"
        } else if size <= 10 * 1024 * 1024 {
            "1-10 MB"
        } else {
            ">10 MB"
        };
        (
            bucket,
            if compressed {
                A2Shape::Zstd(size)
            } else {
                A2Shape::Plain(size)
            },
        )
    }
}

#[test]
fn m7c_r7_sampler_is_seeded_and_has_expected_tail() {
    let mut sampler = R7Sampler::new(0x4d_3743);
    let mut sizes = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let (_, shape) = sampler.next();
        sizes.push(match shape {
            A2Shape::Plain(size) | A2Shape::Zstd(size) => size,
        });
    }
    sizes.sort_unstable();
    assert!(sizes[4_999] <= 6 * 1024 * 1024);
    assert!(sizes[9_499] >= 6 * 1024 * 1024);
    assert!(sizes[9_899] >= 33 * 1024 * 1024);
    assert_eq!(sizes[9_999], 51 * 1024 * 1024);
}
