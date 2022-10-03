// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

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
                .collect::<Vec<_>>();

            // option 1: put an I-frame in front of the first frame
            let l = v.pop().unwrap();
            v.insert(0, Self::IFRAME_SIZE);
            vs.push(v.clone());
            v[0] = Self::IFRAME_LARGE_SIZE;
            vs.push(v.clone());

            // restore the vector
            v.remove(0);
            v.push(l);

            // option 2: first frame is an I-frame and thus large
            v[0] = Self::IFRAME_SIZE;
            vs.push(v.clone());
            v[0] = Self::IFRAME_LARGE_SIZE;
            vs.push(v);

            // rotate
            rv.rotate_right(2);
        }

        RateProbe { probes: vs }
    }

    pub fn iter(&self) -> impl Iterator<Item=&Vec<i64>> {
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
                // 20, 10,  1, 10,  1
                0 => assert!(p[0] == RateProbe::IFRAME_SIZE && p[1] == 10),
                // 50, 10,  1, 10,  1
                1 => assert!(p[1] == 10 && p[2] == 1),
                // 20,  1,  1, 10,  1
                4 => assert!(p[3] == 10 && p[4] == 1),
                _ => (),
            }
        }
    }
}
