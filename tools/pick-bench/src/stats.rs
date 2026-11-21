//! Small summary statistics for the end-of-run table. Deeper analysis
//! (bootstrap intervals, budgets) lives in `scripts/telemetry_summary.py`.

/// Linear-interpolated percentile (`p` in 0..=100) of unsorted values.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = (p / 100.0).clamp(0.0, 1.0) * (v.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    Some(v[lo] + (v[hi] - v[lo]) * (rank - lo as f64))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

pub fn summarize(values: &[f64]) -> Option<Summary> {
    Some(Summary {
        n: values.len(),
        min: percentile(values, 0.0)?,
        p50: percentile(values, 50.0)?,
        p95: percentile(values, 95.0)?,
        max: percentile(values, 100.0)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&v, 50.0), Some(3.0));
        assert_eq!(percentile(&v, 100.0), Some(5.0));
        assert_eq!(percentile(&v, 25.0), Some(2.0));
        assert_eq!(percentile(&[1.0, 2.0], 50.0), Some(1.5));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn summary() {
        let s = summarize(&[10.0, 20.0, 30.0]).unwrap();
        assert_eq!((s.n, s.min, s.p50, s.max), (3, 10.0, 20.0, 30.0));
    }
}
