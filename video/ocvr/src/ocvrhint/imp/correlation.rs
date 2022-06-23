// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

struct Vector<'a> {
    vec: &'a [i64],
    mean: i64,
    std: f64,
}

#[derive(Default)]
pub struct Corr<'a> {
    x: Option<Vector<'a>>,
    y: Option<Vector<'a>>,
}

impl<'a> Corr<'a> {
    pub fn set_x(&mut self, v: &'a [i64]) {
        self.x = Some(Self::set(v));
    }

    pub fn set_y(&mut self, v: &'a [i64]) {
        self.y = Some(Self::set(v));
    }

    pub fn corr(&self) -> Option<f64> {
        if self.x.is_none() || self.y.is_none() {
            return None;
        }

        let mut c = self.cov();

        let x = self.x.as_ref().unwrap();
        let y = self.y.as_ref().unwrap();
        c /= x.std * y.std;

        Some(c.clamp(-1.0, 1.0))
    }

    pub fn corr_y(&mut self, y: &'a [i64]) -> Option<f64> {
        self.set_y(y);
        self.corr()
    }

    fn set(v: &'a [i64]) -> Vector {
        let m = Self::mean(v);
        let s = Self::std(v, m);
        Vector {
            vec: v,
            mean: m,
            std: s,
        }
    }

    fn cov(&self) -> f64 {
        let mut s = 0;
        let x = self.x.as_ref().unwrap();
        let y = self.y.as_ref().unwrap();
        let n = x.vec.len();

        assert!(n == y.vec.len());

        for it in x.vec.iter().zip(y.vec.iter()) {
            let (xi, yi) = it;
            s += (xi - x.mean) * (yi - y.mean);
        }
        (s as f64) / ((n - 1) as f64)
    }

    fn mean(x: &[i64]) -> i64 {
        let s: i64 = x.iter().sum();
        let n = x.len() as f64;
        assert!(n > 0.0);

        ((s as f64) / n) as i64
    }

    fn std(v: &[i64], v_m: i64) -> f64 {
        let n = v.len();
        let mut s = 0;

        for vi in v {
            s += (vi - v_m).pow(2);
        }
        s /= (n as i64) - 1;
        (s as f64).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_corr() {
        let x: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let y: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let mut corr = Corr { x: None, y: None };

        corr.set_x(&x);
        corr.set_y(&y);
        let mut c = corr.corr();
        assert!(c.is_some());
        println!("Correlation of {:?} with {:?} is {}", x, y, c.unwrap());
        assert!(1.0 == c.unwrap());

        let y1 = vec![1, 10, 1, 10, 1, 10];
        c = corr.corr_y(&y1);
        assert!(c.is_some());
        println!("Correlation of {:?} with {:?} is {}", x, y1, c.unwrap());
        assert!(-1.0 == c.unwrap());
    }
}
