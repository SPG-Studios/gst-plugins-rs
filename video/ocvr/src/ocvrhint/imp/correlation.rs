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
        let mut s: f64 = 0.0;
        let x = self.x.as_ref().unwrap();
        let y = self.y.as_ref().unwrap();

        assert_eq!(x.vec.len(), y.vec.len());

        for it in Iterator::zip(x.vec.iter(), y.vec.iter()) {
            let (xi, yi) = it;
            s += (xi - x.mean) as f64 * (yi - y.mean) as f64;
        }
        s / ((x.vec.len() - 1) as f64)
    }

    fn mean(x: &[i64]) -> i64 {
        assert!(!x.is_empty());
        let s = x.iter().sum::<i64>();
        let n = x.len() as f64;

        ((s as f64) / n) as i64
    }

    fn std(v: &[i64], v_m: i64) -> f64 {
        let mut s = v.iter().map(|vi| (vi - v_m).pow(2)).sum::<i64>();
        s /= v.len() as i64 - 1;
        (s as f64).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_corr_equal() {
        let x: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let y: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let mut corr = Corr { x: None, y: None };

        corr.set_x(&x);
        corr.set_y(&y);
        let c = corr.corr();
        assert!(c.is_some());
        assert!(1.0 == c.unwrap());
    }

    #[test]
    fn check_corr_inverse() {
        let x: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let y: Vec<i64> = vec![1, 10, 1, 10, 1, 10];
        let mut corr = Corr { x: None, y: None };

        corr.set_x(&x);
        corr.set_y(&y);
        let c = corr.corr();
        assert!(c.is_some());
        assert!(-1.0 == c.unwrap());
    }

    #[test]
    fn check_corr_scaled_equal() {
        let x: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let y: Vec<i64> = vec![100, 10, 100, 10, 100, 10];
        let mut corr = Corr { x: None, y: None };

        corr.set_x(&x);
        corr.set_y(&y);
        let c = corr.corr();
        assert!(c.is_some());
        assert!(1.0 == c.unwrap());
    }

    #[test]
    fn check_corr_similar() {
        let x: Vec<i64> = vec![10, 1, 10, 1, 10, 1];
        let y: Vec<i64> = vec![10, 2, 8, 3, 9, 5];
        let mut corr = Corr { x: None, y: None };

        corr.set_x(&x);
        corr.set_y(&y);
        let c = corr.corr();
        assert!(c.is_some());
        assert!(0.9 < c.unwrap());
    }
}
