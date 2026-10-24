//! The session lead (S208): the renderers of one ring publish their start
//! lead ℓ and hold-back h in a slot each, and every renderer gives its
//! reader its hold-back plus `K = max(0, max(ℓ − h))`, so the outputs sound
//! exactly their hold-backs apart while none is anchored below its floor.
//!
//! The slots are made with the ring; claiming and releasing one happens on
//! the host's thread, and every other call is a relaxed atomic load or
//! store, safe on an audio thread (S190).

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// How many renderers of one ring count towards the session lead.
pub const SESSION_SLOTS: usize = 32;

/// A slot's lead before its renderer published one.
const NO_LEAD: i64 = i64::MIN;

struct Slot {
    taken: AtomicBool,
    /// Its output's stop was asked for: it no longer counts.
    leaving: AtomicBool,
    lead: AtomicI64,
    hold_back: AtomicI64,
}

/// One slot per renderer of a ring, for the session lead (S208).
pub struct SessionLeads {
    slots: [Slot; SESSION_SLOTS],
}

impl fmt::Debug for SessionLeads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionLeads")
            .field("session_lead", &self.session_lead())
            .finish_non_exhaustive()
    }
}

impl Default for SessionLeads {
    fn default() -> Self {
        SessionLeads::new()
    }
}

impl SessionLeads {
    /// Every slot free.
    pub fn new() -> SessionLeads {
        SessionLeads {
            slots: std::array::from_fn(|_| Slot {
                taken: AtomicBool::new(false),
                leaving: AtomicBool::new(false),
                lead: AtomicI64::new(NO_LEAD),
                hold_back: AtomicI64::new(0),
            }),
        }
    }

    /// Takes a free slot for a renderer at `hold_back` frames; `None` when
    /// all [`SESSION_SLOTS`] are taken. It counts once its lead is set.
    pub fn claim(&self, hold_back: i64) -> Option<usize> {
        let index = self.slots.iter().position(|slot| {
            slot.taken
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        })?;
        let slot = &self.slots[index];
        slot.leaving.store(false, Ordering::Relaxed);
        slot.lead.store(NO_LEAD, Ordering::Relaxed);
        slot.hold_back.store(hold_back, Ordering::Relaxed);
        Some(index)
    }

    /// Publishes the start lead ℓ of `slot`'s renderer, in capture frames.
    pub fn set_lead(&self, slot: usize, frames: i64) {
        self.slots[slot].lead.store(frames, Ordering::Relaxed);
    }

    /// Publishes the hold-back the host asked `slot`'s renderer for.
    pub fn set_hold_back(&self, slot: usize, frames: i64) {
        self.slots[slot].hold_back.store(frames, Ordering::Relaxed);
    }

    /// `slot`'s output is being stopped: it no longer counts.
    pub fn leave(&self, slot: usize) {
        self.slots[slot].leaving.store(true, Ordering::Relaxed);
    }

    /// Frees `slot` for another renderer.
    pub fn release(&self, slot: usize) {
        let slot = &self.slots[slot];
        slot.lead.store(NO_LEAD, Ordering::Relaxed);
        slot.taken.store(false, Ordering::Release);
    }

    /// K, in capture frames: the most any counted renderer's start lead
    /// exceeds its hold-back by, and 0 when none does.
    pub fn session_lead(&self) -> i64 {
        self.slots
            .iter()
            .filter(|slot| !slot.leaving.load(Ordering::Relaxed))
            .map(|slot| {
                let lead = slot.lead.load(Ordering::Relaxed);
                if lead == NO_LEAD {
                    0
                } else {
                    lead.saturating_sub(slot.hold_back.load(Ordering::Relaxed))
                }
            })
            .fold(0, i64::max)
    }

    /// K once every counted renderer has published its start lead, for the
    /// debug log; `None` before that and while no renderer counts.
    pub fn published_session_lead(&self) -> Option<i64> {
        let mut counted = self.slots.iter().filter(|slot| {
            slot.taken.load(Ordering::Acquire) && !slot.leaving.load(Ordering::Relaxed)
        });
        let mut any = false;
        let all = counted.all(|slot| {
            any = true;
            slot.lead.load(Ordering::Relaxed) != NO_LEAD
        });
        (any && all).then(|| self.session_lead())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_lead_is_published_once_every_counted_renderer_reported() {
        let leads = SessionLeads::new();
        assert_eq!(leads.published_session_lead(), None, "no renderer");
        let built_in = leads.claim(0).unwrap();
        let bluetooth = leads.claim(0).unwrap();
        leads.set_lead(built_in, 960);
        assert_eq!(leads.published_session_lead(), None, "one still starting");
        leads.set_lead(bluetooth, 1472);
        assert_eq!(leads.published_session_lead(), Some(1472));
        leads.leave(bluetooth);
        assert_eq!(leads.published_session_lead(), Some(960), "a leaving one");
        leads.release(bluetooth);
        leads.leave(built_in);
        assert_eq!(leads.published_session_lead(), None, "none counts");
    }

    // covers: S208
    #[test]
    fn the_session_lead_is_the_most_a_lead_exceeds_its_hold_back() {
        let leads = SessionLeads::new();
        assert_eq!(leads.session_lead(), 0);
        let built_in = leads.claim(240).unwrap();
        // Counted only once its lead is published.
        assert_eq!(leads.session_lead(), 0);
        leads.set_lead(built_in, 512);
        assert_eq!(leads.session_lead(), 272);
        let bluetooth = leads.claim(0).unwrap();
        leads.set_lead(bluetooth, 1786);
        assert_eq!(leads.session_lead(), 1786);
        // Held back beyond its lead, as for AirPlay: no lead is needed.
        leads.set_hold_back(bluetooth, 84_000);
        leads.set_hold_back(built_in, 96_000);
        assert_eq!(leads.session_lead(), 0);
        leads.set_hold_back(bluetooth, 0);
        leads.leave(bluetooth);
        assert_eq!(leads.session_lead(), 0);
        leads.release(bluetooth);
        leads.set_hold_back(built_in, 0);
        assert_eq!(leads.session_lead(), 512);
        // A released slot is taken again, afresh.
        assert_eq!(leads.claim(0), Some(bluetooth));
        assert_eq!(leads.session_lead(), 512);
    }

    // covers: S208
    #[test]
    fn a_renderer_past_the_last_slot_gets_none() {
        let leads = SessionLeads::new();
        for expected in 0..SESSION_SLOTS {
            assert_eq!(leads.claim(0), Some(expected));
        }
        assert_eq!(leads.claim(0), None);
        leads.release(3);
        assert_eq!(leads.claim(0), Some(3));
    }
}
