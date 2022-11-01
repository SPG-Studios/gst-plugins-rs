// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::VecDeque;

#[derive(Debug)]
pub struct RateProbe {
    probes: Vec<Vec<i64>>,
}

impl RateProbe {
    const IFRAME_SIZE: i64 = 20;
    const IFRAME_LARGE_SIZE: i64 = 50;

    pub fn new(gop_size: usize, vec: &[i64]) -> RateProbe {
        assert!(vec.len() > 1);

        // we rotate the input vector to get all variants and
        // duplicate each with a leading I-frame
        let mut vs: Vec<Vec<i64>> = Vec::with_capacity(vec.len());

        // and we copy the slice into a vector for permutation
        let mut rv = Vec::from(vec);

        for _ in (0..vec.len()).step_by(2) {
            let mut v = rv
                .iter()
                .cycle()
                .take(gop_size)
                .copied()
                .collect::<VecDeque<_>>();

            // option 1: first frame is an I-frame and thus large
            let l = v[0];
            v[0] = Self::IFRAME_SIZE;
            vs.push(Vec::from(v.clone()));
            v[0] = Self::IFRAME_LARGE_SIZE;
            vs.push(Vec::from(v.clone()));
            v[0] = l; // restore original vector

            // option 2: put an I-frame in front of the first frame
            v.pop_back();
            v.push_front(Self::IFRAME_SIZE);
            vs.push(Vec::from(v.clone()));
            v[0] = Self::IFRAME_LARGE_SIZE;
            vs.push(Vec::from(v.clone()));

            // rotate
            rv.rotate_right(2);
        }

        RateProbe { probes: vs }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Vec<i64>> {
        self.probes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_iterate() {
        let gop = 30;
        let probe = RateProbe::new(gop, &[10, 1, 10, 1, 1]);
        assert!(probe.probes.len() == 12);
        for p in probe.iter() {
            assert!(p.len() == gop);
        }
    }

    #[test]
    fn get_and_check_vecs() {
        let probe = RateProbe::new(30, &[10, 1, 10, 1, 1]);
        for (i, p) in probe.iter().enumerate() {
            match i {
                0 => assert_eq!(&p[..=4], &[RateProbe::IFRAME_SIZE, 1, 10, 1, 1]),
                1 => assert_eq!(&p[..=4], &[RateProbe::IFRAME_LARGE_SIZE, 1, 10, 1, 1]),
                2 => assert_eq!(&p[..=4], &[RateProbe::IFRAME_SIZE, 10, 1, 10, 1]),
                3 => assert_eq!(&p[..=4], &[RateProbe::IFRAME_LARGE_SIZE, 10, 1, 10, 1]),
                _ => (),
            }
        }
    }
}
