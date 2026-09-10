use rostra_core::ShortEventId;

use crate::{Announcement, EpochPublic};

/// Durable per-account/per-device reducer; never reconstruct from only retained
/// announcement payloads because retirement is permanent.
#[derive(Clone, Default, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct DeviceState {
    retired: bool,
    latest: Option<(u64, ShortEventId, EpochPublic)>,
}

impl DeviceState {
    /// Apply one already authenticated and validated announcement for this
    /// row's account and device. Retirement dominates every active event.
    pub fn apply(&mut self, announcement: &Announcement, timestamp: u64, event: ShortEventId) {
        if let Some(epoch) = &announcement.epoch {
            if !self.retired
                && self.latest.as_ref().is_none_or(|(old_time, old_event, _)| {
                    (*old_time, *old_event) < (timestamp, event)
                })
            {
                self.latest = Some((timestamp, event, epoch.clone()));
            }
        } else {
            self.retired = true;
            self.latest = None;
        }
    }

    /// Whether any authenticated retirement was observed for this device ID.
    pub fn retired(&self) -> bool {
        self.retired
    }

    /// The newest active announcement, including future or expired keys.
    ///
    /// Selection must precede eligibility. Callers must not fall back to older
    /// events when this key is unavailable.
    pub fn latest(&self) -> Option<(u64, ShortEventId, &EpochPublic)> {
        self.latest
            .as_ref()
            .map(|(time, event, epoch)| (*time, *event, epoch))
    }
}
