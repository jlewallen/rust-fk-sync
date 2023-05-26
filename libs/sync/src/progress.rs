use std::ops::RangeInclusive;

pub struct RangeProgress {
    pub range: RangeInclusive<u64>,
    pub completed: f32,
    pub total: usize,
    pub received: usize,
}

impl std::fmt::Debug for RangeProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:.4} ({}/{}, {:?})",
            self.completed, self.received, self.total, self.range
        ))
    }
}

#[allow(dead_code)]
pub struct Progress {
    pub total: Option<RangeProgress>,
    pub batch: Option<RangeProgress>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.total, &self.batch) {
            (Some(total), Some(batch)) => {
                f.write_fmt(format_args!("T({:?}) B({:?})", total, batch))
            }
            (None, None) => f.write_str("Idle"),
            (_, _) => todo!("Nonsensical progress!"),
        }
    }
}
