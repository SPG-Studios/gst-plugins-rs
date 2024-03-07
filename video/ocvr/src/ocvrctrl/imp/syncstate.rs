// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

use super::CaptureRate;
use super::ContentRate;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SyncState {
    Idle,
    // rate, frame counter, consecutive matching frames
    Syncing(ContentRate, u32, u32),
    SyncLost(ContentRate),
    // current frame index within period
    Hz24(u32),
    Hz25(u32),
    Hz30(u32),
    // current frame index within period, compare match/mismatch
    Hz50(u32, bool),
    Hz60(u32, bool),
}

macro_rules! advance_or_reset {
    ($v: ident, $p: expr) => {{
        *$v += 1;
        if *$v == $p {
            *$v = 0;
        }
    }};
}

macro_rules! advance_matches {
    ($m: ident, $a: expr) => {{
        if $a {
            *$m += 1;
        } else {
            *$m = 0;
        }
    }};
}

impl SyncState {
    // pattern matching explanation
    // 0: new frame
    // x: repeated frame
    // _: frame to save
    // ^: frame to compare with previous frame

    // naming scheme explanation
    // HZxx_yy_PERIOD:        the number of frames in a pattern
    // HZxx_yy_SYNC_PERIOD:   max. number of frames to process while sync'ing
    // HZxx_yy_SYNC_MATCHES:  required number of subsequent matches/mismatches
    //                        for successful sync
    // HZxx_yy_SYNCED_PERIOD: observation period when sync'ed
    // HZxx_yy_SYNCED_START:  position within period when getting into
    //                        sync'ed state

    // pattern: OxOx|OxOx|Ox...
    const HZ25_50_PERIOD: u32 = 2;
    // pattern: |OxOxOxOxOxOx|Ox...
    // check:    _^
    const HZ25_50_SYNCED_PERIOD: u32 = 6 * Self::HZ25_50_PERIOD;
    // index to start with after sync
    const HZ25_50_SYNCED_START: u32 = 1;
    // pattern: OxOxOxOxOxOxOx
    //          _^_^_^
    const HZ25_50_SYNC_PERIOD: u32 = 8;
    // match - mismatch - match - mismatch - match
    const HZ25_50_SYNC_MATCHES: u32 = 5;

    // pattern: OxOx0|OxOx0|Ox...
    const HZ30_50_PERIOD: u32 = 5;
    // pattern: |OxOxOOxOxO|0x...
    // check:    _^   _^
    const HZ30_50_SYNCED_PERIOD: u32 = 2 * Self::HZ30_50_PERIOD;
    // index to start with after sync
    const HZ30_50_SYNCED_START: u32 = 0;
    // pattern: OxOxO0xOx
    //          _^_^_^
    const HZ30_50_SYNC_PERIOD: u32 = 3 * Self::HZ30_50_PERIOD;
    // match - mismatch - match - mismatch - mismatch
    const HZ30_50_SYNC_MATCHES: u32 = 5;

    // pattern: O|O|O|O|O...
    const HZ50_50_PERIOD: u32 = 1;
    // pattern: |OOOOOOOOOO|O0...
    // check :   _^^^
    const HZ50_50_SYNCED_PERIOD: u32 = 10 * Self::HZ50_50_PERIOD;

    // pattern: OxxOx|OxxOx|Oxx...
    const HZ24_60_PERIOD: u32 = 5;
    // pattern: |OxxOxOxxOxOxxOx|Oxx...
    // check:     _^
    const HZ24_60_SYNCED_PERIOD: u32 = 3 * Self::HZ24_60_PERIOD;
    // index to start with after sync
    const HZ24_60_SYNCED_START: u32 = 2;
    // pattern: OxxOxOxxOx
    //          _^^
    const HZ24_60_SYNC_PERIOD: u32 = 8;
    // 2 consecutive matches -> 3 consecutive identical frames
    const HZ24_60_SYNC_MATCHES: u32 = 2;

    // pattern: OxOx|OxOx|Ox...
    const HZ30_60_PERIOD: u32 = 2;
    // pattern: |OxOxOxOxOxOx|Ox...
    // check:    _^
    const HZ30_60_SYNCED_PERIOD: u32 = 6 * Self::HZ30_60_PERIOD;
    // index to start with after sync
    const HZ30_60_SYNCED_START: u32 = 1;
    // pattern: OxOxOxOxOxOxOx
    //          _^_^_^
    const HZ30_60_SYNC_PERIOD: u32 = 8;
    // match - mismatch - match - mismatch - match
    const HZ30_60_SYNC_MATCHES: u32 = 5;

    // pattern: O|O|O|O|O...
    const HZ60_60_PERIOD: u32 = 1;
    // pattern: |OOOOOOOOOO|O0...
    // check :   _^^^
    const HZ60_60_SYNCED_PERIOD: u32 = 10 * Self::HZ60_60_PERIOD;

    pub fn sync_lost(&self) -> bool {
        matches!(self, SyncState::SyncLost(_))
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, SyncState::Idle)
    }

    pub fn reset(&mut self) {
        *self = SyncState::Idle;
    }

    pub fn is_synced(&self) -> bool {
        matches!(
            self,
            SyncState::Hz24(_)
                | SyncState::Hz25(_)
                | SyncState::Hz30(_)
                | SyncState::Hz50(_, _)
                | SyncState::Hz60(_, _)
        )
    }

    pub fn resync(&mut self) {
        *self = match self {
            SyncState::Syncing(r, _, _) => SyncState::Syncing(*r, 0, 0),
            SyncState::Hz24(_) => SyncState::Syncing(ContentRate::Hz24, 0, 0),
            SyncState::Hz25(_) => SyncState::Syncing(ContentRate::Hz25, 0, 0),
            SyncState::Hz30(_) => SyncState::Syncing(ContentRate::Hz30, 0, 0),
            SyncState::Hz50(_, _) => SyncState::Hz50(0, false),
            SyncState::Hz60(_, _) => SyncState::Hz60(0, false),
            SyncState::SyncLost(r) => SyncState::Syncing(*r, 0, 0),
            _ => unreachable!(),
        }
    }

    pub fn sync(rate: ContentRate) -> Self {
        match rate {
            ContentRate::Hint => SyncState::Idle,
            _ => SyncState::Syncing(rate, 0, 0),
        }
    }

    // will never change the state but just its parameters
    pub fn advance(&mut self, rate: CaptureRate) {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => (),
                SyncState::SyncLost(_) => (),
                SyncState::Syncing(_, i, _) => *i += 1,
                SyncState::Hz25(i) => advance_or_reset!(i, Self::HZ25_50_SYNCED_PERIOD),
                SyncState::Hz30(i) => advance_or_reset!(i, Self::HZ30_50_SYNCED_PERIOD),
                SyncState::Hz50(i, _) => advance_or_reset!(i, Self::HZ50_50_SYNCED_PERIOD),
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => (),
                SyncState::SyncLost(_) => (),
                SyncState::Syncing(_, i, _) => *i += 1,
                SyncState::Hz24(i) => advance_or_reset!(i, Self::HZ24_60_SYNCED_PERIOD),
                SyncState::Hz30(i) => advance_or_reset!(i, Self::HZ30_60_SYNCED_PERIOD),
                SyncState::Hz60(i, _) => advance_or_reset!(i, Self::HZ60_60_SYNCED_PERIOD),
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        };
    }

    pub fn eval_compare(&mut self, rate: CaptureRate, m: bool) -> bool {
        // if we lost sync let's try to recover
        if self.sync_lost() {
            return false;
        }

        let mut r = m;
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => (),
                SyncState::SyncLost(_) => (),
                SyncState::Syncing(_, _, _) => (),
                SyncState::Hz25(_) => (),
                SyncState::Hz30(_) => (),
                SyncState::Hz50(i, v) => {
                    if *i == 1 {
                        *v = m; // use the current result for future comparisons
                    }
                    if !(*v) && self.needs_compare(rate) {
                        r = !m; // invert the result if we are looking for mismatch
                    }
                }
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => (),
                SyncState::SyncLost(_) => (),
                SyncState::Syncing(_, _, _) => (),
                SyncState::Hz24(_) => (),
                SyncState::Hz30(_) => (),
                SyncState::Hz60(i, v) => {
                    if *i == 1 {
                        *v = m; // use the current result for future comparisons
                    }
                    if !(*v) && self.needs_compare(rate) {
                        r = !m; // invert the result if we are looking for mismatch
                    }
                }
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
        !self.is_synced() || r
    }

    // returns true if a state change happened
    pub fn update(&mut self, rate: CaptureRate, alike: bool) -> bool {
        let mut ret: bool = false;

        // first handle the comparison result during syncing
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Syncing(ContentRate::Hz25, _, m) => {
                    if *m & 1 == 0 {
                        advance_matches!(m, alike);
                    } else {
                        advance_matches!(m, !alike);
                    }
                }
                SyncState::Syncing(ContentRate::Hz30, _, m) => {
                    if *m == 0 || *m == 2 {
                        advance_matches!(m, alike);
                    } else {
                        advance_matches!(m, !alike);
                    }
                }
                SyncState::Syncing(_, _, m) => advance_matches!(m, alike),
                _ => (),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Syncing(ContentRate::Hz30, _, m) => {
                    if *m & 1 == 0 {
                        advance_matches!(m, alike);
                    } else {
                        advance_matches!(m, !alike);
                    }
                }
                SyncState::Syncing(_, _, m) => advance_matches!(m, alike),
                _ => (),
            },
            _ => unreachable!(),
        };

        // next update the current state if necessary
        *self = match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => SyncState::Idle,
                SyncState::SyncLost(r) => SyncState::SyncLost(*r),
                SyncState::Syncing(ContentRate::Hz25, Self::HZ25_50_SYNC_PERIOD, _) => {
                    ret = true;
                    SyncState::SyncLost(ContentRate::Hz25)
                }
                SyncState::Syncing(ContentRate::Hz30, Self::HZ30_50_SYNC_PERIOD, _) => {
                    ret = true;
                    SyncState::SyncLost(ContentRate::Hz30)
                }
                SyncState::Syncing(ContentRate::Hz25, _, Self::HZ25_50_SYNC_MATCHES) => {
                    ret = true;
                    SyncState::Hz25(Self::HZ25_50_SYNCED_START)
                }
                SyncState::Syncing(ContentRate::Hz30, _, Self::HZ30_50_SYNC_MATCHES) => {
                    ret = true;
                    SyncState::Hz30(Self::HZ30_50_SYNCED_START)
                }
                SyncState::Syncing(ContentRate::Hz50, _, _) => {
                    ret = true;
                    SyncState::Hz50(0, false)
                }
                SyncState::Syncing(r, i, m) => SyncState::Syncing(*r, *i, *m),
                SyncState::Hz25(i) => SyncState::Hz25(*i),
                SyncState::Hz30(i) => SyncState::Hz30(*i),
                SyncState::Hz50(i, m) => SyncState::Hz50(*i, *m),
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => SyncState::Idle,
                SyncState::SyncLost(r) => SyncState::SyncLost(*r),
                SyncState::Syncing(ContentRate::Hz24, Self::HZ24_60_SYNC_PERIOD, _) => {
                    ret = true;
                    SyncState::SyncLost(ContentRate::Hz24)
                }
                SyncState::Syncing(ContentRate::Hz30, Self::HZ30_60_SYNC_PERIOD, _) => {
                    ret = true;
                    SyncState::SyncLost(ContentRate::Hz30)
                }
                SyncState::Syncing(ContentRate::Hz24, _, Self::HZ24_60_SYNC_MATCHES) => {
                    ret = true;
                    SyncState::Hz24(Self::HZ24_60_SYNCED_START)
                }
                SyncState::Syncing(ContentRate::Hz30, _, Self::HZ30_60_SYNC_MATCHES) => {
                    ret = true;
                    SyncState::Hz30(Self::HZ30_60_SYNCED_START)
                }
                SyncState::Syncing(ContentRate::Hz60, _, _) => {
                    ret = true;
                    SyncState::Hz60(0, false)
                }
                SyncState::Syncing(r, i, m) => SyncState::Syncing(*r, *i, *m),
                SyncState::Hz24(i) => SyncState::Hz24(*i),
                SyncState::Hz30(i) => SyncState::Hz30(*i),
                SyncState::Hz60(i, m) => SyncState::Hz60(*i, *m),
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        };

        ret
    }

    pub fn needs_compare(&self, rate: CaptureRate) -> bool {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => true,
                SyncState::Hz25(i) => *i == 1,
                SyncState::Hz30(i) => *i == 1 || *i == 6,
                SyncState::Hz50(i, _) => (1..=3).contains(i),
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => true,
                SyncState::Hz24(i) => *i == 2,
                SyncState::Hz30(i) => *i == 1,
                SyncState::Hz60(i, _) => (1..=3).contains(i),
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn needs_save(&self, rate: CaptureRate) -> bool {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => true,
                SyncState::Hz25(i) => *i == 0 || *i == 1,
                SyncState::Hz30(i) => *i == 0 || *i == 1 || *i == 5 || *i == 6,
                SyncState::Hz50(i, _) => (0..=3).contains(i),
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => true,
                SyncState::Hz24(i) => *i == 1 || *i == 2,
                SyncState::Hz30(i) => *i == 0 || *i == 1,
                SyncState::Hz60(i, _) => (0..=3).contains(i),
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn ts_adjust(&self, rate: CaptureRate, pts: &mut gst::ClockTime) -> bool {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz25(_) => false,
                SyncState::Hz30(i) => {
                    if *i % Self::HZ30_50_PERIOD == 2 {
                        *pts = pts.checked_sub(gst::ClockTime::from_mseconds(7)).unwrap();
                        true
                    } else if *i % Self::HZ30_50_PERIOD == 4 {
                        *pts = pts.checked_sub(gst::ClockTime::from_mseconds(14)).unwrap();
                        true
                    } else {
                        false
                    }
                }
                SyncState::Hz50(_, _) => false,
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz24(i) => {
                    if *i % Self::HZ24_60_PERIOD == 3 {
                        *pts = pts.checked_sub(gst::ClockTime::from_mseconds(6)).unwrap();
                        true
                    } else {
                        false
                    }
                }
                SyncState::Hz30(_) => false,
                SyncState::Hz60(_, _) => false,
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn dur_adjust(&self, rate: CaptureRate, drop: bool, dur: &mut gst::ClockTime) -> bool {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz25(_) => {
                    *dur = gst::ClockTime::SECOND / 25;
                    true
                }
                SyncState::Hz30(_) => {
                    *dur = gst::ClockTime::SECOND / 30;
                    true
                }
                SyncState::Hz50(_, _) => {
                    if drop {
                        *dur = gst::ClockTime::SECOND / 25;
                    }
                    true
                }
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz24(_) => {
                    *dur = gst::ClockTime::SECOND / 24;
                    true
                }
                SyncState::Hz30(_) => {
                    *dur = gst::ClockTime::SECOND / 30;
                    true
                }
                SyncState::Hz60(_, _) => {
                    if drop {
                        *dur = gst::ClockTime::SECOND / 30;
                    }
                    true
                }
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn drop(&self, drop: bool, rate: CaptureRate) -> bool {
        match rate {
            CaptureRate::HZ_50 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz25(i) => *i & 1 != 0,
                SyncState::Hz30(i) => {
                    *i % Self::HZ30_50_PERIOD == 1 || *i % Self::HZ30_50_PERIOD == 3
                }
                SyncState::Hz50(i, _) => drop && *i & 1 != 0,
                _ => unimplemented!(),
            },
            CaptureRate::HZ_60 => match self {
                SyncState::Idle => false,
                SyncState::SyncLost(_) => false,
                SyncState::Syncing(_, _, _) => false,
                SyncState::Hz24(i) => {
                    *i % Self::HZ24_60_PERIOD == 1
                        || *i % Self::HZ24_60_PERIOD == 2
                        || *i % Self::HZ24_60_PERIOD == 4
                }
                SyncState::Hz30(i) => *i & 1 != 0,
                SyncState::Hz60(i, _) => drop && *i & 1 != 0,
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }
}
