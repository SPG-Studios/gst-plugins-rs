// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use std::slice;

#[derive(Debug)]
pub struct RateProbe {
    probes: Vec<Vec<i64>>,
}

impl RateProbe {
    const IFRAME_MULT: i64 = 5;

    pub fn new(gop_size: usize, vecs: &[&[i64]]) -> RateProbe {
        // we make 3 vectors from each incoming vector
        let mut vs: Vec<Vec<i64>> = Vec::with_capacity(3 * vecs.len());

        for p in vecs {
            assert!(p.len() > 1);
            let mut v: Vec<i64> = p.iter().cycle().take(gop_size).copied().collect();

            // take the original vector expanded to GOP size
            vs.push(v.clone());

            // The I-frame might be large and the first frame
            let c = v[0];
            v[0] = Self::IFRAME_MULT * c; // modify
            vs.push(v.clone());
            v[0] = c; // restore

            // The I-frame might be large and before the first frame
            vs.push(vec![Self::IFRAME_MULT * v[0]]);
            // we added one at the beginning so we have to remove one
            // from the end
            v.pop();
            vs.last_mut().unwrap().append(&mut v);
        }

        RateProbe { probes: vs }
    }

    pub fn iter(&self) -> slice::Iter<Vec<i64>> {
        self.probes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_iterate() {
        let gop = 30;
        let probe = RateProbe::new(gop, &[&[10, 1, 10, 1, 1], &[10, 1, 1, 10, 1]]);
        assert!(probe.probes.len() == 6);
        for p in probe.iter() {
            assert!(p.len() == gop);
        }
    }

    #[test]
    fn get_and_check_vecs() {
        let probe = RateProbe::new(30, &[&[10, 1, 10, 1, 1], &[10, 1, 1, 10, 1]]);
        for (i, p) in probe.iter().enumerate() {
            match i {
                0 => assert!(p[0] == 10 && p[1] == 1),
                1 => assert!(p[0] == RateProbe::IFRAME_MULT * 10 && p[1] == 1),
                2 => assert!(p[0] == RateProbe::IFRAME_MULT * 10 && p[1] == 10),
                _ => (),
            }
        }
    }
}
